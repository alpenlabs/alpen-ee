//! The table registry: per-table reflection through the production codecs.
//!
//! Each console-visible table implements [`TableReflect`], which knows how to
//! count, fetch, and scan it with the value decoded exactly as the node decodes
//! it. This keeps the console on the single canonical codec path (the design's
//! core invariant) instead of a drifting hand-rolled decoder.
//!
//! The store is several MDBX environments, each with its own tables, and the
//! registry mirrors that: one [`console_tables!`] block per environment, and
//! [`ee_envs`] listing them in the order `.tables` prints. Adding a table costs
//! one entry in its environment's block and nothing else. The mechanical half — read-txn plumbing,
//! key parsing, and the scan loop with its predicate-abort dance — lives once in [`Reflected`]; the
//! value's shape comes from its [`ValueReflector`](super::reflect::ValueReflector), which defaults
//! to serde and so needs no per-table code at all. A table with an awkward value
//! names a different reflector instead of hand-rolling a decoder.

use std::{fmt, marker::PhantomData, ops::ControlFlow};

use alpen_reth_db::mdbx::{BlockHashByNumber, BlockStateChangesSchema, PublishedCodeHashSchema};
use alpen_store_mdbx::{
    Direction, KeyCodec, MdbxEnv, Reader, Schema, UpgradeCtx, ValueCodec, Writer,
};

use super::{
    key::ConsoleKey,
    mirrors::ProofReceiptMirror,
    reflect::{BytesReflector, MirrorReflector, SerdeReflector, ValueReflector},
    value::{FieldValue, Record},
};
use crate::mdbxdb::{
    AccountStateAtOLEpochSchema, AcctProofIdIndexSchema, AcctProofReceiptSchema, BatchByIdxSchema,
    BatchChunksSchema, BatchIdToIdxSchema, BlockAccessedStateSchema, BlockWitnessSchema,
    BytecodeSchema, ChunkByIdxSchema, ChunkIdToIdxSchema, ChunkProofReceiptSchema,
    ExecBlockFinalizedSchema, ExecBlockPayloadSchema, ExecBlockSchema, ExecBlocksAtHeightSchema,
    L1BroadcastActiveTxNodeSchema, L1BroadcastTxIdSchema, L1BroadcastTxNodeSchema,
    L1BroadcastTxSchema, L1ChunkedEnvelopeSchema, OLBlockAtEpochSchema, ProverTaskSchema,
};

/// Which keys a walk covers.
///
/// A range is only meaningful on a table whose key encoding preserves key
/// order ([`KeyCodec::ORDERED`]); on any other table it is refused, since the
/// cursor would return the wrong rows rather than no rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Range {
    /// The whole table.
    All,
    /// Every key from `from` to `to`, both inclusive, written as `get` takes
    /// them.
    Between {
        /// The lowest key covered.
        from: String,
        /// The highest key covered.
        to: String,
    },
    /// Every key starting with these bytes, written as hex.
    Prefix(String),
}

/// Static description of a table, shown by `.tables` and `.schema`.
#[derive(Clone, Copy, Debug)]
pub struct TableInfo {
    /// The environment the table lives in (`node`, `prover`, `witness`, `da`).
    pub env: &'static str,
    /// The MDBX sub-database name (matches `Schema::NAME`).
    pub name: &'static str,
    /// Human description of the key type.
    pub key_desc: &'static str,
    /// Human description of the value shape.
    pub value_desc: &'static str,
}

/// A table's console reflection.
///
/// Methods take a [`Reader`] borrowed from a short read transaction opened by
/// [`ConsoleDb`](super::ConsoleDb), so the read-txn discipline stays owned by
/// the caller.
pub trait TableReflect: fmt::Debug + Send + Sync {
    /// Static metadata for `.tables`/`.schema`.
    fn info(&self) -> TableInfo;

    /// The number of entries (O(1) via MDBX stat).
    fn count(&self, reader: &Reader<'_>) -> eyre::Result<usize>;

    /// Fetches one record by its textual key (hex for byte/hash keys).
    fn get(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<Option<Record>>;

    /// Walks the keys in `range` in `direction`, decoding each record for
    /// `visit`, which says whether it counts as a match. Stops after `limit`
    /// matches or at the end of the range, and returns the number of matches.
    ///
    /// The visitor sees the whole record — key and value — and keeps whatever
    /// it wants in whatever form it wants, so a count keeps nothing and a scan
    /// converts a match exactly once. The decode loop stays native, and reads
    /// in pages of [`SCAN_PAGE_ROWS`], each its own read transaction.
    fn scan(
        &self,
        env: &MdbxEnv,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&Record) -> eyre::Result<bool>,
    ) -> eyre::Result<usize>;

    /// Walks the keys in `range` in `direction` without decoding a value,
    /// handing each rendered key to `visit`. Stops after `limit` matches or at
    /// the end of the range, and returns the number of matches.
    fn keys(
        &self,
        env: &MdbxEnv,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&str) -> eyre::Result<bool>,
    ) -> eyre::Result<usize>;

    /// Parses a key as typed and renders it back in its one canonical form.
    ///
    /// Staging validates a key the moment it is typed rather than at commit, so
    /// a typo surfaces next to the command that caused it; and it stages the
    /// canonical spelling, so `0x0a` and `0A` are one key to the batch as they
    /// are to the store.
    fn canonical_key(&self, key: &str) -> eyre::Result<String>;

    /// Reports whether a record exists at `key`, without decoding its value.
    fn contains(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<bool>;

    /// Deletes one record by textual key, reporting whether it was present.
    fn delete(&self, writer: &Writer<'_>, key: &str) -> eyre::Result<bool>;

    /// Re-encodes a record's value and writes it at the record's key.
    ///
    /// The value must be one this table produced: it converts straight back
    /// through the table's own codec, so a value from elsewhere fails to
    /// convert rather than being coerced into something that decodes
    /// differently.
    fn put(&self, writer: &Writer<'_>, record: &Record) -> eyre::Result<()>;

    /// Converts `value` to the exact form this table's decoder produces.
    ///
    /// Staging runs this before accepting any write, for two reasons. It lets an
    /// edit be written in the looser form a prompt can express — a variant named
    /// by a plain string, say — and stores the canonical form instead. And it
    /// proves the value round-trips: what is stored decodes back to exactly what
    /// was staged, so a table whose reflector is lossy is refused rather than
    /// having the parts of the record nobody named quietly rewritten.
    fn canonicalize(&self, value: &FieldValue) -> eyre::Result<FieldValue>;
}

/// Generic reflection for any table whose key is a
/// [`ConsoleKey`](super::key::ConsoleKey) and whose value some
/// [`ValueReflector`] can read.
///
/// `R` is the reflection strategy, defaulting to
/// [`SerdeReflector`](super::reflect::SerdeReflector). It is a type parameter
/// rather than a trait bound on the value so one table can diverge without
/// disturbing the blanket coverage every other table relies on.
pub(crate) struct Reflected<S, R = SerdeReflector> {
    info: TableInfo,
    _schema: PhantomData<fn() -> S>,
    _reflector: PhantomData<fn() -> R>,
}

impl<S, R> Reflected<S, R> {
    /// Builds the reflection for a table described by `info`.
    pub(crate) const fn new(info: TableInfo) -> Self {
        Self {
            info,
            _schema: PhantomData,
            _reflector: PhantomData,
        }
    }
}

impl<S, R> fmt::Debug for Reflected<S, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reflected")
            .field("name", &self.info.name)
            .finish()
    }
}

impl<S, R> TableReflect for Reflected<S, R>
where
    S: Schema + Send + Sync,
    S::Key: ConsoleKey,
    R: ValueReflector<S::Value> + Send + Sync,
{
    fn info(&self) -> TableInfo {
        self.info
    }

    fn count(&self, reader: &Reader<'_>) -> eyre::Result<usize> {
        Ok(reader.count::<S>()?)
    }

    fn get(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<Option<Record>> {
        let key = <S::Key as ConsoleKey>::parse(key)?;
        let Some(value) = reader.get::<S>(&key)? else {
            return Ok(None);
        };
        Ok(Some(Record::new(key.render(), R::to_value(&value)?)))
    }

    fn scan(
        &self,
        env: &MdbxEnv,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&Record) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        let window = Window::resolve::<S>(range)?;
        walk_paged::<S>(env, &window, direction, limit, |key, value, ctx| {
            let key = <S::Key as KeyCodec<S>>::decode_key(key)?;
            let value = <S::Value as ValueCodec<S>>::decode_value(value, ctx)?;
            let record = Record::new(key.render(), R::to_value(&value)?);
            visit(&record)
        })
    }

    fn keys(
        &self,
        env: &MdbxEnv,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&str) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        let window = Window::resolve::<S>(range)?;
        walk_paged::<S>(env, &window, direction, limit, |key, _value, _ctx| {
            let key = <S::Key as KeyCodec<S>>::decode_key(key)?;
            visit(&key.render())
        })
    }

    fn canonical_key(&self, key: &str) -> eyre::Result<String> {
        Ok(<S::Key as ConsoleKey>::parse(key)?.render())
    }

    fn contains(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<bool> {
        let key = <S::Key as ConsoleKey>::parse(key)?;
        Ok(reader.contains::<S>(&key)?)
    }

    fn delete(&self, writer: &Writer<'_>, key: &str) -> eyre::Result<bool> {
        let key = <S::Key as ConsoleKey>::parse(key)?;
        Ok(writer.delete::<S>(&key)?)
    }

    fn put(&self, writer: &Writer<'_>, record: &Record) -> eyre::Result<()> {
        let key = <S::Key as ConsoleKey>::parse(&record.key)?;
        let value = R::from_value(&record.value)?;
        Ok(writer.put::<S>(&key, &value)?)
    }

    fn canonicalize(&self, value: &FieldValue) -> eyre::Result<FieldValue> {
        let canonical = R::to_value(&R::from_value(value)?)?;
        // The canonical form must itself survive the trip unchanged, or what is
        // stored would not decode back to what was staged.
        if R::to_value(&R::from_value(&canonical)?)? != canonical {
            eyre::bail!(
                "`{}` does not survive a decode/encode round trip, so this write \
                 could store something that reads back differently; the table is \
                 read-only until its reflector is fixed",
                <S as Schema>::NAME
            );
        }
        Ok(canonical)
    }
}

/// A [`Range`] resolved against one table's key encoding: the encoded bounds a
/// raw walk can be positioned and stopped by.
struct Window {
    /// The lowest encoded key covered, if bounded below.
    lower: Option<Vec<u8>>,
    /// The upper edge, if bounded above.
    upper: Option<Edge>,
}

/// How a window ends.
enum Edge {
    /// At this encoded key, inclusive.
    Key(Vec<u8>),
    /// Where keys stop starting with these bytes.
    Prefix(Vec<u8>),
}

/// Where a key sits relative to a window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Below,
    Inside,
    Above,
}

/// Longer than any key the store uses, so a prefix padded with it bounds
/// every key that starts with the prefix.
const PREFIX_PAD: usize = 64;

impl Window {
    /// Resolves `range` for table `S`, refusing a bounded range on a table
    /// whose key encoding does not preserve order.
    fn resolve<S: Schema>(range: &Range) -> eyre::Result<Self>
    where
        S::Key: ConsoleKey,
    {
        let ordered = || {
            if !<S::Key as KeyCodec<S>>::ORDERED {
                eyre::bail!(
                    "`{}` keys are not stored in key order, so a range over them would \
                     return the wrong rows; scan the whole table instead",
                    <S as Schema>::NAME
                );
            }
            Ok(())
        };
        match range {
            Range::All => Ok(Self {
                lower: None,
                upper: None,
            }),
            Range::Between { from, to } => {
                ordered()?;
                let lower = <S::Key as ConsoleKey>::parse(from)?.encode_key()?;
                let upper = <S::Key as ConsoleKey>::parse(to)?.encode_key()?;
                if lower > upper {
                    eyre::bail!("range is empty: `{from}` is above `{to}`");
                }
                Ok(Self {
                    lower: Some(lower),
                    upper: Some(Edge::Key(upper)),
                })
            }
            Range::Prefix(text) => {
                ordered()?;
                let prefix = <S::Key as ConsoleKey>::prefix(text)?;
                Ok(Self {
                    lower: Some(prefix.clone()),
                    upper: Some(Edge::Prefix(prefix)),
                })
            }
        }
    }

    /// The key a walk in `direction` is positioned at first.
    fn start(&self, direction: Direction) -> Option<Vec<u8>> {
        match direction {
            Direction::Forward => self.lower.clone(),
            Direction::Backward => match &self.upper {
                None => None,
                Some(Edge::Key(key)) => Some(key.clone()),
                // Every key with the prefix sorts at or before the prefix
                // padded out with 0xff, so a backward walk from there lands
                // on the last of them.
                Some(Edge::Prefix(prefix)) => {
                    let mut padded = prefix.clone();
                    padded.resize(prefix.len() + PREFIX_PAD, 0xff);
                    Some(padded)
                }
            },
        }
    }

    fn place(&self, key: &[u8]) -> Place {
        if self
            .lower
            .as_ref()
            .is_some_and(|lower| key < lower.as_slice())
        {
            return Place::Below;
        }
        match &self.upper {
            None => Place::Inside,
            Some(Edge::Key(upper)) if key > upper.as_slice() => Place::Above,
            Some(Edge::Prefix(prefix)) if !key.starts_with(prefix) => Place::Above,
            Some(_) => Place::Inside,
        }
    }
}

/// Rows a walk reads per read transaction.
///
/// A walk is split into pages so that no read transaction stays open for
/// long: a long-lived reader pins the store's snapshot, and a node writing
/// underneath it cannot reclaim pages until the reader ends (see
/// `MdbxConfig`). Between pages the walk resumes after the last key it read,
/// so a scan beside a running node is not one atomic snapshot: a row written
/// or removed while it runs may or may not be seen.
pub const SCAN_PAGE_ROWS: usize = 10_000;

/// Walks `S` raw within `window`, counting the entries `judge` accepts and
/// stopping at `limit` of them, [`SCAN_PAGE_ROWS`] per read transaction.
///
/// `judge` decodes as much of the entry as it needs and returns whether it
/// matched; it gets the page's upgrade context, since a value carries its
/// version tag and an old version's up-converter may read other tables
/// through the same transaction. Its first error ends the walk and is
/// returned as is — the store's own error type never has to carry it.
fn walk_paged<S: Schema>(
    env: &MdbxEnv,
    window: &Window,
    direction: Direction,
    limit: Option<usize>,
    mut judge: impl FnMut(&[u8], &[u8], &UpgradeCtx<'_>) -> eyre::Result<bool>,
) -> eyre::Result<usize> {
    let mut matches = 0;
    let mut resume: Option<Vec<u8>> = None;
    loop {
        let remaining = limit.map(|limit| limit.saturating_sub(matches));
        if remaining == Some(0) {
            break;
        }
        let page = env.view(|reader| {
            walk_page::<S>(
                reader,
                window,
                direction,
                resume.as_deref(),
                remaining,
                &mut judge,
            )
        })?;
        matches += page.matches;
        match page.resume_after {
            Some(key) => resume = Some(key),
            None => break,
        }
    }
    Ok(matches)
}

/// One page of a walk.
struct Page {
    /// Entries `judge` accepted on this page.
    matches: usize,
    /// The last key read, when the page filled before the walk was done;
    /// `None` when the walk reached the end of the window or its limit.
    resume_after: Option<Vec<u8>>,
}

/// Walks up to [`SCAN_PAGE_ROWS`] entries of `S` within `window` in one read
/// transaction, continuing after `resume` when the walk is a resumed one.
fn walk_page<S: Schema>(
    reader: &Reader<'_>,
    window: &Window,
    direction: Direction,
    resume: Option<&[u8]>,
    limit: Option<usize>,
    judge: &mut impl FnMut(&[u8], &[u8], &UpgradeCtx<'_>) -> eyre::Result<bool>,
) -> eyre::Result<Page> {
    let ctx = reader.upgrade_ctx();
    let mut matches = 0;
    let mut seen = 0;
    let mut failed: Option<eyre::Error> = None;
    let mut resume_after = None;
    let start = resume
        .map(<[u8]>::to_vec)
        .or_else(|| window.start(direction));
    reader.walk::<S>(start.as_deref(), direction, |key, value| {
        // A resumed walk is positioned on the key the last page ended with,
        // which that page already judged.
        if resume.is_some_and(|last| last == key) {
            return Ok(ControlFlow::Continue(()));
        }
        // The walk starts at the window's near edge, so a key outside it on
        // the near side is at most the one the cursor snapped to; a key past
        // the far edge ends the walk.
        match (window.place(key), direction) {
            (Place::Inside, _) => {}
            (Place::Above, Direction::Forward) | (Place::Below, Direction::Backward) => {
                return Ok(ControlFlow::Break(()));
            }
            _ => return Ok(ControlFlow::Continue(())),
        }
        seen += 1;
        match judge(key, value, &ctx) {
            Ok(true) => {
                matches += 1;
                if limit.is_some_and(|limit| matches >= limit) {
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(false) => {}
            Err(err) => {
                failed = Some(err);
                return Ok(ControlFlow::Break(()));
            }
        }
        if seen >= SCAN_PAGE_ROWS {
            resume_after = Some(key.to_vec());
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if let Some(err) = failed {
        return Err(err);
    }
    Ok(Page {
        matches,
        resume_after,
    })
}

/// The name a record's key is presented under in the script shell.
///
/// Presentation only: the key is never a field of the value, so this name can
/// collide with a real field without either losing anything.
pub const KEY_FIELD: &str = "key";

/// Declares the console's view of one environment's tables.
///
/// The block names the environment (`in "prover"`) and lists its tables. Each
/// entry names a schema and its operator-facing metadata. The value's shape
/// comes from its reflector, which defaults to serde; a table that needs
/// another strategy names it with `reflector: <Type>`. The generated function
/// returns the reflections in declaration order, which is the order `.tables`
/// prints. An empty block is allowed: the environment is then attached and
/// listed, with nothing reflected in it yet.
macro_rules! console_tables {
    (
        $(#[$fn_docs:meta])*
        $vis:vis fn $name:ident() in $env:literal {
            $(
                $schema:ty => {
                    key: $key_desc:literal,
                    value: $value_desc:literal
                    $(, reflector: $reflector:ty)? $(,)?
                }
            ),* $(,)?
        }
    ) => {
        $(#[$fn_docs])*
        $vis fn $name() -> ::std::vec::Vec<::std::boxed::Box<dyn TableReflect>> {
            ::std::vec![
                $(
                    ::std::boxed::Box::new(
                        Reflected::<$schema $(, $reflector)?>::new(TableInfo {
                            env: $env,
                            name: <$schema as Schema>::NAME,
                            key_desc: $key_desc,
                            value_desc: $value_desc,
                        })
                    ),
                )*
            ]
        }
    };
}

/// One environment the console attaches: its directory name under
/// `<datadir>/mdbx`, and the tables reflected in it.
#[derive(Clone, Copy)]
pub(crate) struct EnvSpec {
    /// The environment's name, which is also its directory under `mdbx/`.
    pub(crate) name: &'static str,
    /// Builds the environment's table reflections.
    pub(crate) tables: fn() -> Vec<Box<dyn TableReflect>>,
}

impl fmt::Debug for EnvSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvSpec").field("name", &self.name).finish()
    }
}

/// The EE store's environments, in the order `.tables` lists them.
///
/// Mirrors the layout `open_ee_db` creates: `node` for chain state every node
/// keeps, and `prover`, `witness`, `da` for the sequencer-only stores. A full
/// node's datadir has only `node`, so the attach treats a missing directory as
/// an absent environment rather than an error.
pub(crate) fn ee_envs() -> Vec<EnvSpec> {
    vec![
        EnvSpec {
            name: "node",
            tables: node_env_tables,
        },
        EnvSpec {
            name: "prover",
            tables: prover_env_tables,
        },
        EnvSpec {
            name: "witness",
            tables: witness_env_tables,
        },
        EnvSpec {
            name: "da",
            tables: da_env_tables,
        },
    ]
}

console_tables! {
    /// Builds the console's view of the node environment's tables: the chain,
    /// its batches and chunks, and the per-block proving cache.
    pub(crate) fn node_env_tables() in "node" {
        OLBlockAtEpochSchema => {
            key: "OL epoch (u32)",
            value: "OL block id (32-byte hash hex)",
        },
        AccountStateAtOLEpochSchema => {
            key: "OL block id (32-byte hash hex)",
            value: "DBAccountStateAtEpoch { epoch, slot, account_state: { last_exec_blkid, last_exec_state_root, pending_inputs, pending_fincls } }",
        },
        ExecBlockSchema => {
            key: "exec block hash (32-byte hex)",
            value: "DBExecBlockRecord { blocknum, parent_blockhash, timestamp_ms, ol_block, package_ssz: bytes, account_state, next_inbox_msg_idx, next_deposit_idx, next_spec_version, messages }",
        },
        ExecBlocksAtHeightSchema => {
            key: "height (u64)",
            value: "[exec block hash hex]",
        },
        ExecBlockFinalizedSchema => {
            key: "height (u64)",
            value: "finalized exec block hash hex",
        },
        ExecBlockPayloadSchema => {
            key: "exec block hash (32-byte hex)",
            value: "payload bytes",
            reflector: BytesReflector,
        },
        BatchByIdxSchema => {
            key: "batch idx (u64)",
            value: "DBBatchWithStatus { batch: { idx, prev_block, last_block, last_blocknum, inner_blocks, spec_version }, status }",
        },
        BatchIdToIdxSchema => {
            key: "DBBatchId (prev_block:last_block hex pair)",
            value: "batch idx (u64)",
        },
        ChunkByIdxSchema => {
            key: "chunk idx (u64)",
            value: "DBChunkWithStatus { chunk: { idx, prev_block, last_block, last_blocknum, batch_idx, inner_blocks }, status }",
        },
        ChunkIdToIdxSchema => {
            key: "DBChunkId (prev_block:last_block hex pair)",
            value: "chunk idx (u64)",
        },
        BatchChunksSchema => {
            key: "DBBatchId (prev_block:last_block hex pair)",
            value: "[DBChunkId { prev_block, last_block }]",
        },
        BlockAccessedStateSchema => {
            key: "exec block hash (32-byte hex)",
            value: "AccessedStateRecord { accounts: [{ address, storage_slots }], bytecode_hashes, ancestor_block_numbers }",
        },
        BytecodeSchema => {
            key: "code hash (32-byte hex)",
            value: "bytecode bytes",
            reflector: BytesReflector,
        },
        BlockWitnessSchema => {
            key: "exec block hash (32-byte hex)",
            value: "witness bytes (codec-encoded EvmPartialState)",
            reflector: BytesReflector,
        },
    }
}

console_tables! {
    /// Builds the console's view of the witness environment's tables, whose
    /// schemas live in `alpen-reth-db`.
    pub(crate) fn witness_env_tables() in "witness" {
        BlockStateChangesSchema => {
            key: "reth block hash (32-byte hex)",
            value: "BlockStateChanges { accounts, storage, deployed_bytecodes }",
        },
        BlockHashByNumber => {
            key: "reth block number (u64)",
            value: "reth block hash hex",
        },
        PublishedCodeHashSchema => {
            key: "code hash (32-byte hex)",
            value: "() — presence only; the key says everything",
        },
    }
}

console_tables! {
    /// Builds the console's view of the DA environment's tables: the L1
    /// broadcast queue, its replacement chains, and the chunked envelopes.
    pub(crate) fn da_env_tables() in "da" {
        L1BroadcastTxIdSchema => {
            key: "broadcast idx (u64)",
            value: "txid (32-byte hex)",
        },
        L1BroadcastTxSchema => {
            key: "txid (32-byte hex)",
            value: "L1TxEntry { tx_raw, status, rbf }",
        },
        L1BroadcastTxNodeSchema => {
            key: "TxNodeId (32-byte hex)",
            value: "TxNodeRecord { node_id, kind, active_txid, attempts, terminal_error }",
        },
        L1BroadcastActiveTxNodeSchema => {
            key: "TxNodeId (32-byte hex)",
            value: "() — presence marks a chain that may still need fee bumping",
        },
        L1ChunkedEnvelopeSchema => {
            key: "envelope idx (u64)",
            value: "ChunkedEnvelopeEntry { chunk_data, magic_bytes, da_blob_version, commit_txid, commit_wtxid, reveals, status }",
        },
    }
}

console_tables! {
    /// Builds the console's view of the prover environment's tables.
    pub(crate) fn prover_env_tables() in "prover" {
        ProverTaskSchema => {
            key: "tag-prefixed ProofSpec::Task bytes ([u8] -> hex)",
            value: "TaskRecordData { status, updated_at_secs, retry_after_secs, metadata }",
        },
        ChunkProofReceiptSchema => {
            key: "chunk task bytes ([u8] -> hex)",
            value: "ProofReceiptWithMetadata { receipt: { proof: bytes, public_values: bytes }, metadata }",
            reflector: MirrorReflector<ProofReceiptMirror>,
        },
        AcctProofReceiptSchema => {
            key: "DBBatchId (prev_block:last_block hex pair)",
            value: "ProofReceiptWithMetadata { receipt: { proof: bytes, public_values: bytes }, metadata }",
            reflector: MirrorReflector<ProofReceiptMirror>,
        },
        AcctProofIdIndexSchema => {
            key: "ProofId (32-byte hash hex)",
            value: "DBBatchId { prev_block, last_block }",
        },
    }
}

#[cfg(test)]
console_tables! {
    /// A big-endian `u64`-keyed table, for tests of ordered ranges.
    pub(crate) fn heights_test_tables() in "node" {
        ExecBlockFinalizedSchema => {
            key: "height (u64)",
            value: "block hash",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is what `.tables` walks, so a table that is declared twice
    /// (or renamed away from its schema) would show up as a phantom entry.
    #[test]
    fn the_prover_registry_matches_the_schema_names() {
        let tables = prover_env_tables();
        let names: Vec<_> = tables.iter().map(|t| t.info().name).collect();
        assert_eq!(
            names,
            vec![
                <ProverTaskSchema as Schema>::NAME,
                <ChunkProofReceiptSchema as Schema>::NAME,
                <AcctProofReceiptSchema as Schema>::NAME,
                <AcctProofIdIndexSchema as Schema>::NAME,
            ]
        );
    }

    #[test]
    fn every_registered_table_describes_itself() {
        for env in ee_envs() {
            for table in (env.tables)() {
                let info = table.info();
                assert_eq!(
                    info.env, env.name,
                    "`{}` is filed under the wrong env",
                    info.name
                );
                assert!(!info.key_desc.is_empty(), "`{}`: no key desc", info.name);
                assert!(
                    !info.value_desc.is_empty(),
                    "`{}`: no value desc",
                    info.name
                );
            }
        }
    }

    /// Environment names double as directory names and as the `env/` prefix of
    /// a qualified table name, so they must be distinct.
    #[test]
    fn environment_names_are_unique() {
        let mut names: Vec<_> = ee_envs().iter().map(|env| env.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ee_envs().len());
    }

    /// A bare table name resolves only while it is unique across environments.
    /// The console can disambiguate with `env/Table`, but the registry should
    /// not quietly grow a collision that makes every bare lookup fail.
    #[test]
    fn table_names_are_unique_across_environments() {
        let mut names: Vec<_> = ee_envs()
            .iter()
            .flat_map(|env| (env.tables)().into_iter().map(|t| t.info().name))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "a table name is registered in two environments"
        );
    }
}
