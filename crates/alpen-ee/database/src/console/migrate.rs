//! What a production sled store maps to, table by table, for the offline
//! migration into MDBX.
//!
//! The sled binary is alpen 0.3.0. It kept one tree per table, named as the
//! table struct, in one sled directory. Since then the MDBX store changed
//! three things, and this module is the one place that knows all of them:
//!
//! - **Encodings.** Twelve tables version their values: on disk a leading version tag, then that
//!   version's payload. Where the sled binary's borsh bytes are exactly version 1's payload, the
//!   value is *tagged*: the tag is prepended and nothing else. Where the payload codec changed (the
//!   prover task, the L1 tx entry and the chunked-envelope entry went from borsh to CBOR) or an
//!   unversioned value changed shape (the witness block hash, the active-tx-node marker), the value
//!   is *re-encoded*: decoded from the sled binary's layout and encoded through the table's own
//!   codec. The L1 txid was borsh on both sides and copies.
//! - **Types.** Two records gained a field since 0.3.0, the exec block's `next_spec_version` and
//!   the batch's `spec_version`, and the prover task status enum changed its variants. Those are
//!   decoded through the sled-era layouts kept beside the current types and converted, with the
//!   spec version in force at 0.3.0 filled in.
//! - **Keys.** The sled binary wrote the prover task key through borsh, which prefixes a byte
//!   string with its length; MDBX stores it raw. That one tree strips the prefix. Every other key
//!   copies byte for byte: integer keys were big-endian on both sides, hashes are raw 32 bytes.
//!
//! Two trees the MDBX store has, the L1 tx node table and its active-marker
//! set, did not exist at 0.3.0 and come up empty; the rules for them stay so
//! a later sled binary's store also migrates.
//!
//! Nothing here reads sled; the operator tool does, as raw bytes, and hands
//! them to [`ConsoleDb::import_raw`](super::ConsoleDb::import_raw). The
//! post-import check decodes every row through its table's codec and version
//! chain, which is what proves each rule produced what the table reads.

use alloy_primitives::B256;
use alpen_reth_db::mdbx::BlockHashByNumber;
use alpen_store_mdbx::{UpgradeCtx, ValueCodec};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::Serialize;
use strata_db_types::{
    chunked_envelope::{ChunkedEnvelopeEntry, ChunkedEnvelopeStatus, RevealTxMeta},
    l1_broadcast::{L1TxEntry, L1TxStatus},
};
use strata_identifiers::{Buf32, RBuf32};
use strata_l1_txfmt::MagicBytes;
use strata_paas::{AttemptCounts, TaskRecordData, TaskStatus};

use crate::{
    mdbxdb::{
        BatchByIdxSchema, ExecBlockSchema, L1BroadcastTxSchema, L1ChunkedEnvelopeSchema,
        ProverTaskSchema,
    },
    serialization_types::{DBBatchWithStatus, DBExecBlockRecord},
};

/// The tag the sled binary's payloads carry once imported: every version
/// chain starts at 1, which the store's macro asserts at compile time.
const SLED_ERA_VERSION: u8 = 1;

/// CBOR's encoding of `()`, which the sled binary stored as a presence marker.
const CBOR_NULL: u8 = 0xf6;

/// What to do with a tree's keys on the way into its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRule {
    /// The bytes are already the table's key encoding.
    Copy,
    /// The sled binary wrote the key through borsh, a little-endian `u32`
    /// length then the bytes; the table stores the bytes alone.
    StripBorshLength,
}

impl KeyRule {
    /// Applies the rule to one key.
    pub fn apply(self, key: &[u8]) -> eyre::Result<Vec<u8>> {
        match self {
            Self::Copy => Ok(key.to_vec()),
            Self::StripBorshLength => {
                let (prefix, rest) = key.split_first_chunk::<4>().ok_or_else(|| {
                    eyre::eyre!("key of {} bytes has no length prefix", key.len())
                })?;
                let declared = u32::from_le_bytes(*prefix) as usize;
                if declared != rest.len() {
                    eyre::bail!(
                        "key length prefix says {declared} bytes but {} follow",
                        rest.len()
                    );
                }
                Ok(rest.to_vec())
            }
        }
    }

    /// The inverse, for tests that build a sled store from an MDBX one.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn invert(self, key: &[u8]) -> Vec<u8> {
        match self {
            Self::Copy => key.to_vec(),
            Self::StripBorshLength => {
                let mut out = (key.len() as u32).to_le_bytes().to_vec();
                out.extend_from_slice(key);
                out
            }
        }
    }
}

/// What to do with a tree's values on the way into its table.
#[derive(Clone, Copy, Debug)]
pub enum ValueRule {
    /// The bytes are already the table's encoding.
    Copy,
    /// The bytes are version 1's payload; the table stores them behind a
    /// version tag.
    Tag,
    /// The bytes are the sled binary's encoding; this turns them into the
    /// table's.
    Recode(fn(&[u8]) -> eyre::Result<Vec<u8>>),
}

impl ValueRule {
    /// Applies the rule to one value.
    pub fn apply(self, value: &[u8]) -> eyre::Result<Vec<u8>> {
        match self {
            Self::Copy => Ok(value.to_vec()),
            Self::Tag => {
                let mut tagged = Vec::with_capacity(value.len() + 1);
                tagged.push(SLED_ERA_VERSION);
                tagged.extend_from_slice(value);
                Ok(tagged)
            }
            Self::Recode(convert) => convert(value),
        }
    }

    /// Whether the rule changes bytes.
    pub fn recodes(self) -> bool {
        !matches!(self, Self::Copy)
    }

    /// One word for a report line.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Copy => "copied",
            Self::Tag => "tagged",
            Self::Recode(_) => "re-encoded",
        }
    }
}

/// One sled tree and the MDBX table it fills.
#[derive(Clone, Copy, Debug)]
pub struct SledTree {
    /// The tree's name in the sled store.
    pub tree: &'static str,
    /// The table, as `env/Table`.
    pub table: &'static str,
    /// What happens to the keys.
    pub key: KeyRule,
    /// What happens to the values.
    pub value: ValueRule,
}

/// Every table the MDBX store has, with the sled tree it comes from.
pub fn sled_trees() -> Vec<SledTree> {
    let entry = |tree: &'static str, table: &'static str, value: ValueRule| SledTree {
        tree,
        table,
        key: KeyRule::Copy,
        value,
    };
    let copy = |tree, table| entry(tree, table, ValueRule::Copy);
    let tag = |tree, table| entry(tree, table, ValueRule::Tag);
    let recode = |tree, table, f| entry(tree, table, ValueRule::Recode(f));
    vec![
        // node: big-endian integer keys and raw hashes on both sides. The
        // record types are versioned now; two of them also grew a field.
        copy("OLBlockAtEpochSchema", "node/OLBlockAtEpochSchema"),
        tag(
            "AccountStateAtOLEpochSchema",
            "node/AccountStateAtOLEpochSchema",
        ),
        recode("ExecBlockSchema", "node/ExecBlockSchema", exec_block_record),
        copy("ExecBlocksAtHeightSchema", "node/ExecBlocksAtHeightSchema"),
        copy("ExecBlockFinalizedSchema", "node/ExecBlockFinalizedSchema"),
        copy("ExecBlockPayloadSchema", "node/ExecBlockPayloadSchema"),
        recode(
            "BatchByIdxSchema",
            "node/BatchByIdxSchema",
            batch_with_status,
        ),
        copy("BatchIdToIdxSchema", "node/BatchIdToIdxSchema"),
        tag("ChunkByIdxSchema", "node/ChunkByIdxSchema"),
        copy("ChunkIdToIdxSchema", "node/ChunkIdToIdxSchema"),
        copy("BatchChunksSchema", "node/BatchChunksSchema"),
        tag("BlockAccessedStateSchema", "node/BlockAccessedStateSchema"),
        copy("BytecodeSchema", "node/BytecodeSchema"),
        copy("BlockWitnessSchema", "node/BlockWitnessSchema"),
        // prover: the task key was written through borsh and its record
        // changed both codec and shape; the receipts are borsh on both sides.
        SledTree {
            tree: "ProverTaskSchema",
            table: "prover/ProverTaskSchema",
            key: KeyRule::StripBorshLength,
            value: ValueRule::Recode(prover_task),
        },
        tag("ChunkProofReceiptSchema", "prover/ChunkProofReceiptSchema"),
        tag("AcctProofReceiptSchema", "prover/AcctProofReceiptSchema"),
        copy("AcctProofIdIndexSchema", "prover/AcctProofIdIndexSchema"),
        // witness: the diff is bincode on both sides and versioned now; the
        // hash-by-number value was the 32 raw bytes; the published code hash
        // marker was already empty.
        tag("BlockStateChangesSchema", "witness/BlockStateChangesSchema"),
        recode(
            "BlockHashByNumber",
            "witness/BlockHashByNumber",
            block_hash_by_number,
        ),
        copy("PublishedCodeHashSchema", "witness/PublishedCodeHashSchema"),
        // da: renamed. The txid is borsh on both sides; the tx and envelope
        // entries were borsh and are CBOR now; the tx node tables did not
        // exist yet, and a later sled binary's marker was CBOR null.
        copy("BcastL1TxIdSchema", "da/L1BroadcastTxIdSchema"),
        recode("BcastL1TxSchema", "da/L1BroadcastTxSchema", l1_tx_entry),
        tag("BcastL1TxNodeSchema", "da/L1BroadcastTxNodeSchema"),
        recode(
            "BcastActiveL1TxNodeSchema",
            "da/L1BroadcastActiveTxNodeSchema",
            active_tx_node_marker,
        ),
        recode(
            "ChunkedEnvelopeSchema",
            "da/L1ChunkedEnvelopeSchema",
            chunked_envelope,
        ),
    ]
}

// --- Re-encodes ------------------------------------------------------------

/// sled stored the block hash as its 32 raw bytes; the table stores it
/// through bincode, which prefixes a byte string with its length.
fn block_hash_by_number(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    if raw.len() != 32 {
        eyre::bail!("block hash value is {} bytes, expected 32", raw.len());
    }
    let hash = B256::from_slice(raw);
    Ok(<B256 as ValueCodec<BlockHashByNumber>>::encode_value(
        &hash,
    )?)
}

/// sled stored the presence marker as CBOR's encoding of `()`, one `null`
/// byte; the table's value is the unit type now and occupies no bytes.
fn active_tx_node_marker(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    match raw {
        [] | [CBOR_NULL] => Ok(Vec::new()),
        other => eyre::bail!(
            "active tx node marker is {} bytes, expected CBOR null or nothing",
            other.len()
        ),
    }
}

/// The exec block record, minus the spec version it has since gained.
fn exec_block_record(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    let record = DBExecBlockRecord::from_sled_era(raw)
        .map_err(|e| eyre::eyre!("exec block record is not the sled binary's layout: {e}"))?;
    Ok(<DBExecBlockRecord as ValueCodec<ExecBlockSchema>>::encode_value(&record)?)
}

/// The batch, minus the spec version it has since gained.
fn batch_with_status(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    let batch = DBBatchWithStatus::from_sled_era(raw)
        .map_err(|e| eyre::eyre!("batch is not the sled binary's layout: {e}"))?;
    Ok(<DBBatchWithStatus as ValueCodec<BatchByIdxSchema>>::encode_value(&batch)?)
}

/// The prover task record as the sled binary stored it, through borsh.
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraTaskRecord {
    status: SledEraTaskStatus,
    updated_at_secs: u64,
    retry_after_secs: Option<u64>,
    metadata: Option<Vec<u8>>,
}

/// The task status enum at 0.3.0: one retry counter, no blocked state.
#[derive(BorshSerialize, BorshDeserialize)]
enum SledEraTaskStatus {
    Pending,
    Proving { retry_count: u32 },
    Completed,
    TransientFailure { retry_count: u32, error: String },
    PermanentFailure { error: String },
}

/// A single retry counter becomes the resume-class count; the other classes
/// did not exist and start at zero.
fn counts_from_retry(retry_count: u32) -> AttemptCounts {
    AttemptCounts {
        retry: retry_count,
        resubmit: 0,
        recheck: 0,
    }
}

/// The record's serde shape, which is how a [`TaskRecordData`] with a chosen
/// timestamp is built: the type keeps its fields private and stamps "now" in
/// its constructor, but its `Deserialize` takes exactly these fields.
#[derive(Serialize)]
struct TaskRecordShape<'a> {
    status: &'a TaskStatus,
    updated_at_secs: u64,
    retry_after_secs: Option<u64>,
    #[serde(with = "serde_bytes")]
    metadata: Option<Vec<u8>>,
}

fn task_record(
    status: TaskStatus,
    updated_at_secs: u64,
    retry_after_secs: Option<u64>,
    metadata: Option<Vec<u8>>,
) -> eyre::Result<TaskRecordData> {
    let shape = TaskRecordShape {
        status: &status,
        updated_at_secs,
        retry_after_secs,
        metadata,
    };
    let mut cbor = Vec::new();
    ciborium::into_writer(&shape, &mut cbor)?;
    Ok(ciborium::from_reader(cbor.as_slice())?)
}

/// The prover task: borsh then, CBOR now, and the status enum changed shape.
fn prover_task(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    let old = SledEraTaskRecord::try_from_slice(raw)
        .map_err(|e| eyre::eyre!("prover task is not the sled binary's layout: {e}"))?;
    let status = match old.status {
        SledEraTaskStatus::Pending => TaskStatus::Pending,
        SledEraTaskStatus::Proving { retry_count } => TaskStatus::Proving {
            counts: counts_from_retry(retry_count),
        },
        SledEraTaskStatus::Completed => TaskStatus::Completed,
        SledEraTaskStatus::TransientFailure { retry_count, error } => {
            TaskStatus::TransientFailure {
                counts: counts_from_retry(retry_count),
                error,
            }
        }
        SledEraTaskStatus::PermanentFailure { error } => TaskStatus::PermanentFailure { error },
    };
    let record = task_record(
        status,
        old.updated_at_secs,
        old.retry_after_secs,
        old.metadata,
    )?;
    Ok(<TaskRecordData as ValueCodec<ProverTaskSchema>>::encode_value(&record)?)
}

/// The L1 tx entry as the sled binary stored it, through borsh: no
/// replacement info, no `Replaced` status.
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraL1TxEntry {
    tx_raw: Vec<u8>,
    status: SledEraL1TxStatus,
}

#[derive(BorshSerialize, BorshDeserialize)]
enum SledEraL1TxStatus {
    Unpublished,
    Published,
    Confirmed {
        confirmations: u64,
        block_hash: [u8; 32],
        block_height: u32,
    },
    Finalized {
        confirmations: u64,
        block_hash: [u8; 32],
        block_height: u32,
    },
    InvalidInputs,
}

/// The L1 tx entry: borsh then, CBOR now, with replacement fields added.
fn l1_tx_entry(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    let old = SledEraL1TxEntry::try_from_slice(raw)
        .map_err(|e| eyre::eyre!("L1 tx entry is not the sled binary's layout: {e}"))?;
    let status = match old.status {
        SledEraL1TxStatus::Unpublished => L1TxStatus::Unpublished,
        SledEraL1TxStatus::Published => L1TxStatus::Published,
        SledEraL1TxStatus::Confirmed {
            confirmations,
            block_hash,
            block_height,
        } => L1TxStatus::Confirmed {
            confirmations,
            block_hash: Buf32(block_hash),
            block_height,
        },
        SledEraL1TxStatus::Finalized {
            confirmations,
            block_hash,
            block_height,
        } => L1TxStatus::Finalized {
            confirmations,
            block_hash: Buf32(block_hash),
            block_height,
        },
        SledEraL1TxStatus::InvalidInputs => L1TxStatus::InvalidInputs,
    };
    let entry = L1TxEntry::from_raw_parts(old.tx_raw, status, None);
    Ok(<L1TxEntry as ValueCodec<L1BroadcastTxSchema>>::encode_value(&entry)?)
}

/// The chunked-envelope entry as the sled binary stored it, through borsh.
/// Same fields as now; the identifiers are their raw bytes.
#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraChunkedEnvelope {
    chunk_data: Vec<Vec<u8>>,
    magic_bytes: [u8; 4],
    da_blob_version: u32,
    commit_txid: [u8; 32],
    commit_wtxid: [u8; 32],
    reveals: Vec<SledEraRevealTxMeta>,
    status: SledEraChunkedEnvelopeStatus,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct SledEraRevealTxMeta {
    vout_index: u32,
    txid: [u8; 32],
    wtxid: [u8; 32],
    tx_bytes: Vec<u8>,
}

#[derive(BorshSerialize, BorshDeserialize)]
enum SledEraChunkedEnvelopeStatus {
    Unsigned,
    Unpublished,
    CommitPublished,
    Published,
    Confirmed,
    Finalized,
    NeedsResign,
}

fn envelope_status(old: SledEraChunkedEnvelopeStatus) -> ChunkedEnvelopeStatus {
    match old {
        SledEraChunkedEnvelopeStatus::Unsigned => ChunkedEnvelopeStatus::Unsigned,
        SledEraChunkedEnvelopeStatus::Unpublished => ChunkedEnvelopeStatus::Unpublished,
        SledEraChunkedEnvelopeStatus::CommitPublished => ChunkedEnvelopeStatus::CommitPublished,
        SledEraChunkedEnvelopeStatus::Published => ChunkedEnvelopeStatus::Published,
        SledEraChunkedEnvelopeStatus::Confirmed => ChunkedEnvelopeStatus::Confirmed,
        SledEraChunkedEnvelopeStatus::Finalized => ChunkedEnvelopeStatus::Finalized,
        SledEraChunkedEnvelopeStatus::NeedsResign => ChunkedEnvelopeStatus::NeedsResign,
    }
}

/// The chunked-envelope entry: borsh then, CBOR now, same shape.
fn chunked_envelope(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    let old = SledEraChunkedEnvelope::try_from_slice(raw)
        .map_err(|e| eyre::eyre!("chunked envelope is not the sled binary's layout: {e}"))?;
    let mut entry = ChunkedEnvelopeEntry::new_unsigned(
        old.chunk_data,
        MagicBytes::from(old.magic_bytes),
        old.da_blob_version,
    );
    entry.commit_txid = RBuf32(old.commit_txid);
    entry.commit_wtxid = RBuf32(old.commit_wtxid);
    entry.reveals = old
        .reveals
        .into_iter()
        .map(|r| RevealTxMeta {
            vout_index: r.vout_index,
            txid: RBuf32(r.txid),
            wtxid: RBuf32(r.wtxid),
            tx_bytes: r.tx_bytes,
        })
        .collect();
    entry.status = envelope_status(old.status);
    Ok(<ChunkedEnvelopeEntry as ValueCodec<
        L1ChunkedEnvelopeSchema,
    >>::encode_value(&entry)?)
}

// --- Inverses, for tests that build a sled store from an MDBX one ----------

/// Turns a table's key bytes into the form the sled binary stored.
#[cfg(any(test, feature = "test-utils"))]
pub fn to_sled_key(table: &str, key: &[u8]) -> eyre::Result<Vec<u8>> {
    Ok(rule_for(table)?.key.invert(key))
}

/// Turns a table's value bytes into the form the sled binary stored, the
/// inverse of the rule. Errors for a value the sled binary could not have
/// written: a later version tag, a spec version past 0.3.0, a task or L1
/// status that did not exist then.
#[cfg(any(test, feature = "test-utils"))]
pub fn to_sled_form(table: &str, value: &[u8]) -> eyre::Result<Vec<u8>> {
    let ctx = UpgradeCtx::detached();
    match (rule_for(table)?.value, table) {
        (ValueRule::Copy, _) => Ok(value.to_vec()),
        (ValueRule::Tag, _) => match value.split_first() {
            Some((&SLED_ERA_VERSION, payload)) => Ok(payload.to_vec()),
            Some((tag, _)) => {
                eyre::bail!("`{table}` value is version {tag}, which the sled binary never wrote")
            }
            None => eyre::bail!("`{table}` value has no version tag"),
        },
        (ValueRule::Recode(_), "witness/BlockHashByNumber") => {
            let hash = <B256 as ValueCodec<BlockHashByNumber>>::decode_value(value, &ctx)?;
            Ok(hash.as_slice().to_vec())
        }
        (ValueRule::Recode(_), "da/L1BroadcastActiveTxNodeSchema") => {
            if !value.is_empty() {
                eyre::bail!(
                    "`{table}` marker should be empty, got {} bytes",
                    value.len()
                );
            }
            Ok(vec![CBOR_NULL])
        }
        (ValueRule::Recode(_), "node/ExecBlockSchema") => {
            let record =
                <DBExecBlockRecord as ValueCodec<ExecBlockSchema>>::decode_value(value, &ctx)?;
            record
                .to_sled_era()
                .ok_or_else(|| eyre::eyre!("exec block carries a spec version past 0.3.0"))
        }
        (ValueRule::Recode(_), "node/BatchByIdxSchema") => {
            let batch =
                <DBBatchWithStatus as ValueCodec<BatchByIdxSchema>>::decode_value(value, &ctx)?;
            batch
                .to_sled_era()
                .ok_or_else(|| eyre::eyre!("batch carries a spec version past 0.3.0"))
        }
        (ValueRule::Recode(_), "prover/ProverTaskSchema") => {
            let record =
                <TaskRecordData as ValueCodec<ProverTaskSchema>>::decode_value(value, &ctx)?;
            let retry_only = |counts: &AttemptCounts| -> eyre::Result<u32> {
                if counts.resubmit != 0 || counts.recheck != 0 {
                    eyre::bail!("task counts beyond retries did not exist at 0.3.0");
                }
                Ok(counts.retry)
            };
            let status = match record.status() {
                TaskStatus::Pending => SledEraTaskStatus::Pending,
                TaskStatus::Proving { counts } => SledEraTaskStatus::Proving {
                    retry_count: retry_only(counts)?,
                },
                TaskStatus::Completed => SledEraTaskStatus::Completed,
                TaskStatus::TransientFailure { counts, error } => {
                    SledEraTaskStatus::TransientFailure {
                        retry_count: retry_only(counts)?,
                        error: error.clone(),
                    }
                }
                TaskStatus::PermanentFailure { error } => SledEraTaskStatus::PermanentFailure {
                    error: error.clone(),
                },
                TaskStatus::Blocked { .. } => eyre::bail!("a blocked task did not exist at 0.3.0"),
            };
            Ok(borsh::to_vec(&SledEraTaskRecord {
                status,
                updated_at_secs: record.updated_at_secs(),
                retry_after_secs: record.retry_after_secs(),
                metadata: record.metadata().map(<[u8]>::to_vec),
            })?)
        }
        (ValueRule::Recode(_), "da/L1BroadcastTxSchema") => {
            let entry = <L1TxEntry as ValueCodec<L1BroadcastTxSchema>>::decode_value(value, &ctx)?;
            if entry.rbf.is_some() {
                eyre::bail!("replacement info did not exist at 0.3.0");
            }
            let status = match &entry.status {
                L1TxStatus::Unpublished => SledEraL1TxStatus::Unpublished,
                L1TxStatus::Published => SledEraL1TxStatus::Published,
                L1TxStatus::Confirmed {
                    confirmations,
                    block_hash,
                    block_height,
                } => SledEraL1TxStatus::Confirmed {
                    confirmations: *confirmations,
                    block_hash: block_hash.0,
                    block_height: *block_height,
                },
                L1TxStatus::Finalized {
                    confirmations,
                    block_hash,
                    block_height,
                } => SledEraL1TxStatus::Finalized {
                    confirmations: *confirmations,
                    block_hash: block_hash.0,
                    block_height: *block_height,
                },
                L1TxStatus::InvalidInputs => SledEraL1TxStatus::InvalidInputs,
                L1TxStatus::Replaced { .. } => {
                    eyre::bail!("a replaced tx did not exist at 0.3.0")
                }
            };
            Ok(borsh::to_vec(&SledEraL1TxEntry {
                tx_raw: entry.tx_raw().to_vec(),
                status,
            })?)
        }
        (ValueRule::Recode(_), "da/L1ChunkedEnvelopeSchema") => {
            let entry =
                <ChunkedEnvelopeEntry as ValueCodec<L1ChunkedEnvelopeSchema>>::decode_value(
                    value, &ctx,
                )?;
            let status = match entry.status {
                ChunkedEnvelopeStatus::Unsigned => SledEraChunkedEnvelopeStatus::Unsigned,
                ChunkedEnvelopeStatus::Unpublished => SledEraChunkedEnvelopeStatus::Unpublished,
                ChunkedEnvelopeStatus::CommitPublished => {
                    SledEraChunkedEnvelopeStatus::CommitPublished
                }
                ChunkedEnvelopeStatus::Published => SledEraChunkedEnvelopeStatus::Published,
                ChunkedEnvelopeStatus::Confirmed => SledEraChunkedEnvelopeStatus::Confirmed,
                ChunkedEnvelopeStatus::Finalized => SledEraChunkedEnvelopeStatus::Finalized,
                ChunkedEnvelopeStatus::NeedsResign => SledEraChunkedEnvelopeStatus::NeedsResign,
            };
            Ok(borsh::to_vec(&SledEraChunkedEnvelope {
                chunk_data: entry.chunk_data().map(<[u8]>::to_vec).collect(),
                magic_bytes: entry.magic_bytes.into(),
                da_blob_version: entry.da_blob_version,
                commit_txid: entry.commit_txid.0,
                commit_wtxid: entry.commit_wtxid.0,
                reveals: entry
                    .reveals
                    .iter()
                    .map(|r| SledEraRevealTxMeta {
                        vout_index: r.vout_index,
                        txid: r.txid.0,
                        wtxid: r.wtxid.0,
                        tx_bytes: r.tx_bytes.clone(),
                    })
                    .collect(),
                status,
            })?)
        }
        (ValueRule::Recode(_), other) => eyre::bail!("no inverse rule for `{other}`"),
    }
}

#[cfg(any(test, feature = "test-utils"))]
fn rule_for(table: &str) -> eyre::Result<SledTree> {
    sled_trees()
        .into_iter()
        .find(|entry| entry.table == table)
        .ok_or_else(|| eyre::eyre!("no sled tree feeds `{table}`"))
}

#[cfg(test)]
mod tests {
    use alpen_reth_db::mdbx::BlockStateChangesSchema;
    use alpen_reth_statediff::BlockStateChanges;
    use alpen_store_mdbx::VersionedTable;

    use super::{super::registry::ee_envs, *};
    use crate::mdbxdb::L1BroadcastActiveTxNodeSchema;

    /// Every table the store has is fed by exactly one tree, and no tree
    /// names a table the store does not have.
    #[test]
    fn the_mapping_covers_every_table_exactly_once() {
        let mut fed: Vec<&str> = sled_trees().iter().map(|t| t.table).collect();
        let mut registered: Vec<String> = ee_envs()
            .iter()
            .flat_map(|env| {
                (env.tables)()
                    .into_iter()
                    .map(move |t| format!("{}/{}", env.name, t.info().name))
            })
            .collect();
        fed.sort_unstable();
        registered.sort_unstable();
        assert_eq!(fed, registered);

        let mut trees: Vec<&str> = sled_trees().iter().map(|t| t.tree).collect();
        let total = trees.len();
        trees.sort_unstable();
        trees.dedup();
        assert_eq!(trees.len(), total, "a tree feeds two tables");
    }

    /// A versioned table whose payload did not change is tagged; one whose
    /// type or codec changed is re-encoded. This pins the split, so a table
    /// gaining a version chain or a field changes the mapping on purpose.
    #[test]
    fn tagged_and_recoded_tables_are_the_expected_ones() {
        let by_kind = |wanted: fn(&ValueRule) -> bool| -> Vec<&str> {
            sled_trees()
                .into_iter()
                .filter(|entry| wanted(&entry.value))
                .map(|entry| entry.table)
                .collect()
        };
        assert_eq!(
            by_kind(|v| matches!(v, ValueRule::Tag)),
            [
                "node/AccountStateAtOLEpochSchema",
                "node/ChunkByIdxSchema",
                "node/BlockAccessedStateSchema",
                "prover/ChunkProofReceiptSchema",
                "prover/AcctProofReceiptSchema",
                "witness/BlockStateChangesSchema",
                "da/L1BroadcastTxNodeSchema",
            ]
        );
        assert_eq!(
            by_kind(|v| matches!(v, ValueRule::Recode(_))),
            [
                "node/ExecBlockSchema",
                "node/BatchByIdxSchema",
                "prover/ProverTaskSchema",
                "witness/BlockHashByNumber",
                "da/L1BroadcastTxSchema",
                "da/L1BroadcastActiveTxNodeSchema",
                "da/L1ChunkedEnvelopeSchema",
            ]
        );
        let prefixed: Vec<&str> = sled_trees()
            .into_iter()
            .filter(|entry| entry.key == KeyRule::StripBorshLength)
            .map(|entry| entry.table)
            .collect();
        assert_eq!(prefixed, ["prover/ProverTaskSchema"]);
    }

    #[test]
    fn the_key_rule_strips_a_borsh_length_and_puts_it_back() {
        let raw = [0x61u8; 65];
        let sled = KeyRule::StripBorshLength.invert(&raw);
        assert_eq!(&sled[..4], &65u32.to_le_bytes());
        assert_eq!(KeyRule::StripBorshLength.apply(&sled).unwrap(), raw);
        assert!(KeyRule::StripBorshLength.apply(&[1, 2]).is_err());
        let mut wrong = sled.clone();
        wrong[0] = 64;
        assert!(KeyRule::StripBorshLength.apply(&wrong).is_err());
    }

    /// A tagged table: the sled payload gets the version-1 tag and nothing
    /// else, the result is exactly what the table writes today and what its
    /// version chain decodes, and the inverse strips the tag.
    #[test]
    fn the_tag_rule_produces_what_the_table_writes_and_reads() {
        let value = BlockStateChanges::default();
        let stored =
            <BlockStateChanges as ValueCodec<BlockStateChangesSchema>>::encode_value(&value)
                .unwrap();
        assert_eq!(stored[0], SLED_ERA_VERSION);

        let sled = to_sled_form("witness/BlockStateChangesSchema", &stored).unwrap();
        assert_eq!(sled, stored[1..]);
        let tagged = ValueRule::Tag.apply(&sled).unwrap();
        assert_eq!(tagged, stored);
        let decoded =
            BlockStateChangesSchema::decode_tagged(&tagged, &UpgradeCtx::detached()).unwrap();
        assert_eq!(
            <BlockStateChanges as ValueCodec<BlockStateChangesSchema>>::encode_value(&decoded)
                .unwrap(),
            stored
        );

        assert!(to_sled_form("witness/BlockStateChangesSchema", &[2, 0]).is_err());
        assert!(to_sled_form("witness/BlockStateChangesSchema", &[]).is_err());
    }

    /// The prover task: a 0.3.0 record in borsh becomes the tagged CBOR the
    /// table reads, with the retry count carried into the attempt counts and
    /// the timestamp preserved; the inverse gives the same borsh back.
    #[test]
    fn the_prover_task_recode_converts_the_status_and_keeps_the_rest() {
        let old = SledEraTaskRecord {
            status: SledEraTaskStatus::TransientFailure {
                retry_count: 3,
                error: "boom".into(),
            },
            updated_at_secs: 1_700_000_000,
            retry_after_secs: Some(1_700_000_060),
            metadata: Some(vec![9, 9]),
        };
        let sled = borsh::to_vec(&old).unwrap();
        let stored = prover_task(&sled).unwrap();
        let record = <TaskRecordData as ValueCodec<ProverTaskSchema>>::decode_value(
            &stored,
            &UpgradeCtx::detached(),
        )
        .unwrap();
        assert_eq!(
            record.status(),
            &TaskStatus::TransientFailure {
                counts: AttemptCounts {
                    retry: 3,
                    resubmit: 0,
                    recheck: 0
                },
                error: "boom".into()
            }
        );
        assert_eq!(record.updated_at_secs(), 1_700_000_000);
        assert_eq!(record.retry_after_secs(), Some(1_700_000_060));
        assert_eq!(record.metadata(), Some(&[9u8, 9][..]));
        assert_eq!(
            to_sled_form("prover/ProverTaskSchema", &stored).unwrap(),
            sled
        );
        assert!(prover_task(b"nope").is_err());
    }

    /// The L1 tx entry: a confirmed 0.3.0 entry becomes the tagged CBOR the
    /// table reads, with no replacement info; the inverse gives the borsh back.
    #[test]
    fn the_l1_tx_recode_converts_the_status() {
        let old = SledEraL1TxEntry {
            tx_raw: vec![0xde, 0xad],
            status: SledEraL1TxStatus::Confirmed {
                confirmations: 3,
                block_hash: [7; 32],
                block_height: 42,
            },
        };
        let sled = borsh::to_vec(&old).unwrap();
        let stored = l1_tx_entry(&sled).unwrap();
        let entry = <L1TxEntry as ValueCodec<L1BroadcastTxSchema>>::decode_value(
            &stored,
            &UpgradeCtx::detached(),
        )
        .unwrap();
        assert_eq!(entry.tx_raw(), &[0xde, 0xad]);
        assert_eq!(
            entry.status,
            L1TxStatus::Confirmed {
                confirmations: 3,
                block_hash: Buf32([7; 32]),
                block_height: 42
            }
        );
        assert!(entry.rbf.is_none());
        assert_eq!(
            to_sled_form("da/L1BroadcastTxSchema", &stored).unwrap(),
            sled
        );
    }

    /// The chunked envelope: same shape, borsh then, CBOR now.
    #[test]
    fn the_chunked_envelope_recode_round_trips() {
        let old = SledEraChunkedEnvelope {
            chunk_data: vec![vec![1, 2], vec![3]],
            magic_bytes: *b"ALPN",
            da_blob_version: 1,
            commit_txid: [1; 32],
            commit_wtxid: [2; 32],
            reveals: vec![SledEraRevealTxMeta {
                vout_index: 1,
                txid: [3; 32],
                wtxid: [4; 32],
                tx_bytes: vec![5, 6],
            }],
            status: SledEraChunkedEnvelopeStatus::Published,
        };
        let sled = borsh::to_vec(&old).unwrap();
        let stored = chunked_envelope(&sled).unwrap();
        let entry = <ChunkedEnvelopeEntry as ValueCodec<L1ChunkedEnvelopeSchema>>::decode_value(
            &stored,
            &UpgradeCtx::detached(),
        )
        .unwrap();
        assert_eq!(entry.chunk_count(), 2);
        assert_eq!(entry.status, ChunkedEnvelopeStatus::Published);
        assert_eq!(entry.reveals.len(), 1);
        assert_eq!(entry.reveals[0].txid, RBuf32([3; 32]));
        assert_eq!(
            to_sled_form("da/L1ChunkedEnvelopeSchema", &stored).unwrap(),
            sled
        );
    }

    #[test]
    fn the_marker_recode_turns_cbor_null_into_nothing() {
        assert_eq!(
            active_tx_node_marker(&[CBOR_NULL]).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(active_tx_node_marker(&[]).unwrap(), Vec::<u8>::new());
        assert!(active_tx_node_marker(&[1]).is_err());
        <() as ValueCodec<L1BroadcastActiveTxNodeSchema>>::decode_value(
            &active_tx_node_marker(&[CBOR_NULL]).unwrap(),
            &UpgradeCtx::detached(),
        )
        .unwrap();
        assert_eq!(
            to_sled_form("da/L1BroadcastActiveTxNodeSchema", &[]).unwrap(),
            vec![CBOR_NULL]
        );
        assert!(to_sled_form("da/L1BroadcastActiveTxNodeSchema", &[1]).is_err());
    }

    #[test]
    fn the_block_hash_reencode_produces_what_the_table_decodes() {
        let raw = [7u8; 32];
        let encoded = block_hash_by_number(&raw).unwrap();
        assert_ne!(encoded, raw.to_vec(), "bincode prefixes a length");
        let decoded = <B256 as ValueCodec<BlockHashByNumber>>::decode_value(
            &encoded,
            &UpgradeCtx::detached(),
        )
        .unwrap();
        assert_eq!(decoded, B256::from(raw));
        assert_eq!(
            to_sled_form("witness/BlockHashByNumber", &encoded).unwrap(),
            raw
        );
        assert!(block_hash_by_number(&[1, 2, 3]).is_err());
    }
}
