//! Per-table value versioning: version-dispatch on read, current format on write.
//!
//! Every versioned value is **self-describing**: its on-disk bytes are a leading
//! 1-byte version tag followed by that version's payload. The store keeps no
//! per-table schema state at all — version currency lives entirely in the tag,
//! so opening a store is instant and needs no migration pass, no background job,
//! and no bookkeeping table.
//!
//! # The read path
//!
//! ```text
//! raw bytes
//!   -> read tag t
//!   -> decode with the decoder this table registered for tag t  // -> concrete V_t
//!   -> up-convert chain V_t -> V_{t+1} -> ... -> V_current  // -> current type
//! ```
//!
//! Writes always emit the current version with the current tag. Reads never
//! write back, so a read stays a read: cold keys keep their old format on disk
//! until the application naturally writes them again.
//!
//! # Declaring a versioned table
//!
//! A table names its shipped versions ascending, the last one current. Each
//! entry says how that version's payload is encoded (`borsh`, `bincode`,
//! `cbor`, or `codec`), which is what generates its [`SchemaVersion`] impl; the
//! converters between consecutive versions are ordinary [`UpConvert`] impls.
//!
//! The chain belongs to the table, so adding a version to one table leaves every
//! other table's stored bytes untouched.
//!
//! ```
//! use alpen_store_mdbx::{define_table_versioned, CodecError, UpConvert, UpgradeCtx};
//! use borsh::{BorshDeserialize, BorshSerialize};
//!
//! #[derive(BorshSerialize, BorshDeserialize)]
//! pub struct AccountStateV1 {
//!     pub balance: u64,
//! }
//!
//! #[derive(BorshSerialize, BorshDeserialize)]
//! pub struct AccountStateV2 {
//!     pub balance: u64,
//!     pub nonce: u64,
//! }
//!
//! impl UpConvert<AccountStateV2> for AccountStateV1 {
//!     fn up_convert(self, _ctx: &UpgradeCtx<'_>) -> Result<AccountStateV2, CodecError> {
//!         Ok(AccountStateV2 {
//!             balance: self.balance,
//!             nonce: 0,
//!         })
//!     }
//! }
//!
//! define_table_versioned! {
//!     /// EE account state by account id.
//!     (AccountStates) [u8; 32] => {
//!         1 => AccountStateV1 as borsh,
//!         2 => AccountStateV2 as borsh,
//!     }
//! }
//! ```
//!
//! Bumping the version means *adding* a struct and *adding* one converter.
//! Shipped structs and converters are never edited: values carrying their tags
//! are still on disk. A missing `N -> N+1` converter is a compile error, because
//! the generated chain calls [`UpConvert`] for each consecutive pair.
//!
//! A version may be a type this crate does not own — a store's records
//! frequently are. The impls hang off the table type, which the declaring crate
//! always owns, so no local wrapper is needed to satisfy coherence.
//!
//! # Decoder retirement
//!
//! There is no background sweep, so a table never converges on its own and an
//! old decoder can never be safely dropped — a cold key may hold `v1` forever.
//! Keep every decoder, keep decode total over `[first, current]`, and keep
//! [golden fixtures](fixtures) for every version ever shipped: live traffic
//! only ever exercises the newest ones.

use std::fmt;

use crate::codec::{BoxError, CodecError, KeyCodec, Schema, ValueCodec};

/// How deep an up-converter's context reads may nest before the read is refused.
///
/// Up-converters must not form read cycles ([`UpgradeCtx`]). This bound turns a
/// cycle that slipped through review into a clean error instead of a stack
/// overflow.
pub const MAX_UPGRADE_DEPTH: u8 = 8;

/// Raw, untyped read access to the ambient transaction, for [`UpgradeCtx`].
///
/// Implemented by the store's transaction types; this indirection is what keeps
/// [`UpgradeCtx`] free of the transaction's kind parameter.
pub trait RawGet {
    /// Fetches the raw stored bytes for `key` in the sub-database `table`.
    fn get_raw(&self, table: &'static str, key: &[u8]) -> Result<Option<Vec<u8>>, BoxError>;
}

/// The transaction an up-converter may read while decoding a value.
///
/// An up-converter is not a pure `fn(old) -> new`: it runs inside the ambient
/// transaction and may derive a new field from *other* state, reading it through
/// the same version-dispatching accessor (so referenced rows are themselves
/// up-converted).
///
/// Two rules keep this sound:
///
/// 1. **No cycles.** `A`'s up-converter reading `B` while `B`'s reads `A` is forbidden; nesting
///    past [`MAX_UPGRADE_DEPTH`] is refused.
/// 2. **Stable context, or materialize.** A read-path up-converter must be a deterministic function
///    of *(old self, the transaction snapshot)*. An up-converter deriving a value from **mutable**
///    context would recompute a different result once that context changes, so it must persist the
///    derived value forward on the next write rather than rely on recomputation. Defaulting, or
///    deriving from immutable data, is safe to recompute forever.
pub struct UpgradeCtx<'txn> {
    txn: Option<&'txn dyn RawGet>,
    depth: u8,
}

impl fmt::Debug for UpgradeCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpgradeCtx")
            .field("attached", &self.txn.is_some())
            .field("depth", &self.depth)
            .finish()
    }
}

impl<'txn> UpgradeCtx<'txn> {
    /// Builds a context bound to a transaction.
    pub fn new(txn: &'txn dyn RawGet) -> Self {
        Self {
            txn: Some(txn),
            depth: 0,
        }
    }

    /// Builds a context with no transaction behind it.
    ///
    /// Decoding still works for every up-converter that only defaults or derives
    /// from `self`; one that reads another table fails with
    /// [`CodecError::NoUpgradeContext`]. Used for decoding loose bytes, e.g. the
    /// golden-fixture harness.
    pub fn detached() -> Self {
        Self {
            txn: None,
            depth: 0,
        }
    }

    /// How many context reads deep this decode already is.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Reads another table through the same version-dispatching accessor.
    ///
    /// The referenced value is decoded — and therefore up-converted — exactly as
    /// a normal read would decode it.
    pub fn get<S: Schema>(&self, key: &S::Key) -> Result<Option<S::Value>, CodecError> {
        let txn = self
            .txn
            .ok_or(CodecError::NoUpgradeContext { schema: S::NAME })?;

        if self.depth >= MAX_UPGRADE_DEPTH {
            return Err(CodecError::UpgradeContextDepth {
                schema: S::NAME,
                depth: self.depth,
            });
        }

        let key_bytes = key.encode_key()?;
        let raw =
            txn.get_raw(S::NAME, &key_bytes)
                .map_err(|source| CodecError::UpgradeContextRead {
                    schema: S::NAME,
                    source,
                })?;

        match raw {
            Some(bytes) => {
                let nested = UpgradeCtx {
                    txn: self.txn,
                    depth: self.depth + 1,
                };
                Ok(Some(<S::Value as ValueCodec<S>>::decode_value(
                    &bytes, &nested,
                )?))
            }
            None => Ok(None),
        }
    }
}

/// One shipped on-disk version of the value stored in table `S`.
///
/// Implementations are generated by the `as <codec>` clause on a chain entry in
/// [`impl_versioned_value_codec!`](crate::impl_versioned_value_codec), or
/// written by hand when the entry omits it. Once a version has shipped, neither
/// its struct nor its codec may change: bytes carrying its tag are still on
/// disk.
///
/// The table parameter is what lets a version be a type the declaring crate does
/// not own: the table type is always local to it.
pub trait SchemaVersion<S>: Sized {
    /// This version's on-disk tag.
    const VERSION: u8;

    /// Decodes this version's payload (the bytes *after* the version tag).
    fn decode_payload(bytes: &[u8]) -> Result<Self, CodecError>;

    /// Encodes this version's payload (the bytes *after* the version tag).
    fn encode_payload(&self) -> Result<Vec<u8>, CodecError>;
}

/// Converts one shipped version to the next one in the chain.
///
/// Implemented once per consecutive pair, and never edited afterwards.
pub trait UpConvert<To>: Sized {
    /// Converts `self` forward one version, optionally reading other tables
    /// through `ctx` (see [`UpgradeCtx`] for the rules that keeps sound).
    fn up_convert(self, ctx: &UpgradeCtx<'_>) -> Result<To, CodecError>;
}

/// Folds a shipped version of table `S`'s value up to the type `S` stores now.
///
/// Generated by
/// [`impl_versioned_value_codec!`](crate::impl_versioned_value_codec) by
/// chaining [`UpConvert`] across every consecutive pair, so a missing edge fails
/// the build rather than a decode.
pub trait LiftToCurrent<S: Schema>: Sized {
    /// Runs the up-convert chain from `self` to the current version.
    fn lift_to_current(self, ctx: &UpgradeCtx<'_>) -> Result<S::Value, CodecError>;
}

/// A table whose stored values are a version tag followed by that version's
/// payload.
///
/// Each table owns its own chain, so adding a version to one leaves every other
/// table's stored bytes untouched. [`Schema::Value`] is whichever version is
/// current.
pub trait VersionedTable: Schema {
    /// The tag this binary writes.
    const CURRENT_VERSION: u8;

    /// Every version this binary can decode, ascending, ending at
    /// [`Self::CURRENT_VERSION`].
    const VERSIONS: &'static [u8];

    /// Decodes tagged bytes, dispatching on the tag and folding up to current.
    fn decode_tagged(bytes: &[u8], ctx: &UpgradeCtx<'_>) -> Result<Self::Value, CodecError>;

    /// Encodes to the current version, tagged.
    fn encode_tagged(value: &Self::Value) -> Result<Vec<u8>, CodecError>;
}

/// Splits stored bytes into their version tag and payload.
///
/// Public because
/// [`impl_versioned_value_codec!`](crate::impl_versioned_value_codec) expands
/// into it.
pub fn split_version_tag<'a>(
    table: &'static str,
    bytes: &'a [u8],
) -> Result<(u8, &'a [u8]), CodecError> {
    match bytes.split_first() {
        Some((tag, payload)) => Ok((*tag, payload)),
        None => Err(CodecError::MissingVersionTag { schema: table }),
    }
}

/// Builds the error for a tag no decoder claims, distinguishing "written by a
/// newer binary" from a gap in the chain.
///
/// Public because
/// [`impl_versioned_value_codec!`](crate::impl_versioned_value_codec) expands
/// into it.
pub fn unknown_version_error(table: &'static str, tag: u8, current: u8) -> CodecError {
    if tag > current {
        CodecError::NewerVersion {
            schema: table,
            tag,
            current,
        }
    } else {
        CodecError::UnknownVersion { schema: table, tag }
    }
}

pub mod fixtures {
    //! Golden fixtures: archived bytes of every historical version.
    //!
    //! Live operation only ever writes the current version, so an old
    //! up-converter runs solely when a store that still holds old bytes is read
    //! — and a bug in one can hide for months. Keeping a real encoded sample of
    //! every shipped version and replaying it in CI is the invariant that keeps
    //! the never-crash-on-an-old-format guarantee true over time; the rest of
    //! the design is easy to state and easy to let rot.

    use super::{Schema, UpgradeCtx, VersionedTable};
    use crate::codec::CodecError;

    /// One archived encoding of a historical version, as it sits on disk
    /// (version tag included).
    ///
    /// Real fixtures are files pulled in with `include_bytes!`, which gives them
    /// a `'static` lifetime; the borrow is left open so tests can also build
    /// them in memory.
    #[derive(Clone, Copy, Debug)]
    pub struct GoldenFixture<'a> {
        /// The version these bytes were written at.
        pub version: u8,
        /// The full stored bytes, leading tag included.
        pub bytes: &'a [u8],
    }

    impl<'a> GoldenFixture<'a> {
        /// Builds a fixture for `version` from its stored bytes.
        pub const fn new(version: u8, bytes: &'a [u8]) -> Self {
            Self { version, bytes }
        }
    }

    /// Reports a fixture set that does not cover every version a store may hold.
    #[derive(Debug, thiserror::Error)]
    pub enum FixtureError {
        /// A version this binary can decode has no archived sample.
        #[error("`{table}`: no golden fixture for version {version}")]
        MissingVersion {
            /// The table the fixture belongs to.
            table: &'static str,
            /// The version with no fixture.
            version: u8,
        },

        /// A fixture claims a version the table does not declare.
        #[error("`{table}`: golden fixture claims unknown version {version}")]
        UnknownVersion {
            /// The table the fixture belongs to.
            table: &'static str,
            /// The version the fixture claims.
            version: u8,
        },

        /// A fixture's declared version disagrees with its leading tag.
        #[error("`{table}`: golden fixture for version {version} is tagged {tag}")]
        TagMismatch {
            /// The table the fixture belongs to.
            table: &'static str,
            /// The version the fixture claims.
            version: u8,
            /// The tag actually found in the bytes.
            tag: u8,
        },

        /// A fixture failed to decode and fold up to the current version.
        #[error("`{table}`: golden fixture for version {version} failed to decode")]
        Decode {
            /// The table the fixture belongs to.
            table: &'static str,
            /// The version that failed.
            version: u8,
            /// The decode failure.
            #[source]
            source: CodecError,
        },
    }

    /// Checks that `fixtures` cover every version of table `S` exactly once, and
    /// that each one decodes and folds up to the current version.
    ///
    /// Returns the decoded values, in fixture order, so a caller can assert on
    /// the up-converted contents too.
    ///
    /// `ctx` is the context the up-converters run in. Pass
    /// [`UpgradeCtx::detached`] for a table whose converters only default or
    /// derive from `self`; one that reads other tables needs a fixture store
    /// populated in a transaction.
    pub fn check_fixtures<S: VersionedTable>(
        fixtures: &[GoldenFixture<'_>],
        ctx: &UpgradeCtx<'_>,
    ) -> Result<Vec<S::Value>, FixtureError> {
        let table = <S as Schema>::NAME;

        for fixture in fixtures {
            if !S::VERSIONS.contains(&fixture.version) {
                return Err(FixtureError::UnknownVersion {
                    table,
                    version: fixture.version,
                });
            }
        }

        for version in S::VERSIONS {
            if !fixtures.iter().any(|f| f.version == *version) {
                return Err(FixtureError::MissingVersion {
                    table,
                    version: *version,
                });
            }
        }

        // Every fixture is checked against its own leading tag before anything
        // decodes, so a mislabelled sample is reported as such rather than as
        // whatever its decoder happens to complain about.
        for fixture in fixtures {
            match fixture.bytes.first() {
                Some(tag) if *tag == fixture.version => {}
                Some(tag) => {
                    return Err(FixtureError::TagMismatch {
                        table,
                        version: fixture.version,
                        tag: *tag,
                    })
                }
                None => {
                    return Err(FixtureError::Decode {
                        table,
                        version: fixture.version,
                        source: CodecError::MissingVersionTag { schema: table },
                    })
                }
            }
        }

        fixtures
            .iter()
            .map(|fixture| {
                S::decode_tagged(fixture.bytes, ctx).map_err(|source| FixtureError::Decode {
                    table,
                    version: fixture.version,
                    source,
                })
            })
            .collect()
    }
}

/// Behavioural tests for per-table value versioning.
///
/// The store's guarantee is that a binary decodes every format it has ever
/// written, refuses anything newer without misreading it, and never blocks
/// startup to do so. These exercise that from the outside: old bytes are placed
/// on disk through a raw view of the same sub-database, then read back through
/// the normal typed accessor.
#[cfg(test)]
mod tests {
    use borsh::{BorshDeserialize, BorshSerialize};
    use tempfile::tempdir;

    use crate::{
        define_table_borsh, impl_borsh_key_codec, impl_raw_value_codec, tables,
        version::fixtures::{check_fixtures, FixtureError, GoldenFixture},
        CodecError, DbError, MdbxConfig, MdbxEnv, Schema, SchemaVersion, UpConvert, UpgradeCtx,
        VersionedTable, MAX_UPGRADE_DEPTH,
    };

    type Hash = [u8; 32];

    // --- A three-version value, the last converter reading another table ----

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub(crate) struct AccountV1 {
        balance: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub(crate) struct AccountV2 {
        balance: u64,
        nonce: u64,
        owner: Hash,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub(crate) struct AccountV3 {
        balance: u64,
        nonce: u64,
        owner: Hash,
        code_hash: Hash,
    }

    impl UpConvert<AccountV2> for AccountV1 {
        fn up_convert(self, _ctx: &UpgradeCtx<'_>) -> Result<AccountV2, CodecError> {
            Ok(AccountV2 {
                balance: self.balance,
                nonce: 0,
                owner: [0; 32],
            })
        }
    }

    impl UpConvert<AccountV3> for AccountV2 {
        /// Derives the new field from another table, through the same
        /// version-dispatching accessor.
        fn up_convert(self, ctx: &UpgradeCtx<'_>) -> Result<AccountV3, CodecError> {
            let code_hash = ctx.get::<Codes>(&self.owner)?.unwrap_or([0; 32]);
            Ok(AccountV3 {
                balance: self.balance,
                nonce: self.nonce,
                owner: self.owner,
                code_hash,
            })
        }
    }

    crate::define_table_versioned! {
        /// Accounts, version-dispatched on read and written at the current version.
        (Accounts) Hash => {
            1 => AccountV1 as borsh,
            2 => AccountV2 as borsh,
            3 => AccountV3 as borsh,
        }
    }

    define_table_borsh! {
        /// Owner to code hash, read by the v2 -> v3 up-converter.
        (Codes) Hash => Hash
    }

    /// A raw view of the `Accounts` sub-database, for planting bytes an older
    /// binary would have written and for inspecting the tag actually on disk.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct AccountsRaw;

    impl Schema for AccountsRaw {
        const NAME: &'static str = "Accounts";
        type Key = Hash;
        type Value = Vec<u8>;
    }
    impl_borsh_key_codec!(AccountsRaw, Hash);
    impl_raw_value_codec!(AccountsRaw);

    // --- A value whose up-converter reads its own table (a forbidden cycle) ---

    #[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
    pub(crate) struct LoopyV1 {
        key: Hash,
    }

    #[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
    pub(crate) struct LoopyV2 {
        key: Hash,
    }

    impl UpConvert<LoopyV2> for LoopyV1 {
        fn up_convert(self, ctx: &UpgradeCtx<'_>) -> Result<LoopyV2, CodecError> {
            ctx.get::<Loopies>(&self.key)?;
            Ok(LoopyV2 { key: self.key })
        }
    }

    crate::define_table_versioned! {
        /// Table backing the cycle test: its up-converter reads its own table.
        (Loopies) Hash => {
            1 => LoopyV1 as borsh,
            2 => LoopyV2 as borsh,
        }
    }

    // --- A version whose payload codec is written by hand ---------------------

    /// A payload in an encoding no `as` clause names: a marker byte then the bytes
    /// verbatim. The family entry therefore omits the clause and the impl below
    /// stands in for it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct TallyV1 {
        bytes: Vec<u8>,
    }

    /// Marks a payload this codec wrote, so a decode of foreign bytes fails rather
    /// than returning something plausible.
    const TALLY_MARKER: u8 = 0xa5;

    impl SchemaVersion<Tallies> for TallyV1 {
        const VERSION: u8 = 1;

        fn decode_payload(bytes: &[u8]) -> Result<Self, CodecError> {
            match bytes.split_first() {
                Some((&TALLY_MARKER, rest)) => Ok(Self {
                    bytes: rest.to_vec(),
                }),
                _ => Err(CodecError::decode("Tallies", "missing marker byte")),
            }
        }

        fn encode_payload(&self) -> Result<Vec<u8>, CodecError> {
            let mut out = vec![TALLY_MARKER];
            out.extend_from_slice(&self.bytes);
            Ok(out)
        }
    }

    crate::define_table_versioned! {
        /// Table whose only version brings its own `SchemaVersion` impl.
        (Tallies) Hash => {
            1 => TallyV1,
        }
    }

    /// A raw view of the `Tallies` sub-database, for inspecting the bytes the table
    /// actually wrote.
    pub(crate) struct TalliesRaw;

    impl Schema for TalliesRaw {
        const NAME: &'static str = "Tallies";
        type Key = Hash;
        type Value = Vec<u8>;
    }
    impl_borsh_key_codec!(TalliesRaw, Hash);
    impl_raw_value_codec!(TalliesRaw);

    // --- Helpers --------------------------------------------------------------

    fn open() -> (tempfile::TempDir, MdbxEnv) {
        let dir = tempdir().unwrap();
        let env = MdbxEnv::open(
            dir.path(),
            &MdbxConfig::small(),
            &tables![Accounts, Codes, Loopies, Tallies],
        )
        .unwrap();
        (dir, env)
    }

    /// Encodes a payload the way a binary shipping only that version would have.
    fn tagged(version: u8, payload: &impl BorshSerialize) -> Vec<u8> {
        let mut bytes = vec![version];
        bytes.extend_from_slice(&borsh::to_vec(payload).unwrap());
        bytes
    }

    const KEY: Hash = [1; 32];
    const OWNER: Hash = [0xaa; 32];
    const CODE: Hash = [0xbb; 32];

    // --- Version dispatch on read --------------------------------------------

    #[test]
    fn old_value_decodes_and_folds_up_to_current() {
        let (_dir, env) = open();

        // What a v1-era binary wrote.
        let old = tagged(1, &AccountV1 { balance: 7 });
        env.update(|w| w.put::<AccountsRaw>(&KEY, &old)).unwrap();

        let account = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
        assert_eq!(
            account,
            AccountV3 {
                balance: 7,
                nonce: 0,
                owner: [0; 32],
                code_hash: [0; 32],
            }
        );
    }

    #[test]
    fn up_converter_derives_a_field_from_another_table() {
        let (_dir, env) = open();

        env.update::<_, DbError>(|w| {
            w.put::<Codes>(&OWNER, &CODE)?;
            w.put::<AccountsRaw>(
                &KEY,
                &tagged(
                    2,
                    &AccountV2 {
                        balance: 9,
                        nonce: 3,
                        owner: OWNER,
                    },
                ),
            )
        })
        .unwrap();

        let account = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
        assert_eq!(account.code_hash, CODE);
        assert_eq!(account.nonce, 3);
    }

    #[test]
    fn read_does_not_write_back_and_cold_keys_stay_old() {
        let (_dir, env) = open();
        let cold: Hash = [2; 32];

        env.update::<_, DbError>(|w| {
            w.put::<AccountsRaw>(&KEY, &tagged(1, &AccountV1 { balance: 1 }))?;
            w.put::<AccountsRaw>(&cold, &tagged(1, &AccountV1 { balance: 2 }))
        })
        .unwrap();

        // Reading both up-converts in memory only.
        env.view::<_, DbError>(|r| {
            r.get::<Accounts>(&KEY)?;
            r.get::<Accounts>(&cold)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            env.view(|r| r.get::<AccountsRaw>(&KEY)).unwrap().unwrap()[0],
            1,
            "a read must not rewrite the value"
        );

        // A natural write drifts that one key forward; the cold key stays at v1.
        let touched = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
        env.update(|w| w.put::<Accounts>(&KEY, &touched)).unwrap();

        assert_eq!(
            env.view(|r| r.get::<AccountsRaw>(&KEY)).unwrap().unwrap()[0],
            3,
            "a write lands in the current format"
        );
        assert_eq!(
            env.view(|r| r.get::<AccountsRaw>(&cold)).unwrap().unwrap()[0],
            1,
            "an untouched key keeps its old format"
        );
    }

    #[test]
    fn scans_dispatch_per_value_across_mixed_versions() {
        let (_dir, env) = open();

        env.update::<_, DbError>(|w| {
            w.put::<Codes>(&OWNER, &CODE)?;
            w.put::<AccountsRaw>(&[1; 32], &tagged(1, &AccountV1 { balance: 1 }))?;
            w.put::<AccountsRaw>(
                &[2; 32],
                &tagged(
                    2,
                    &AccountV2 {
                        balance: 2,
                        nonce: 0,
                        owner: OWNER,
                    },
                ),
            )?;
            w.put::<Accounts>(
                &[3; 32],
                &AccountV3 {
                    balance: 3,
                    nonce: 0,
                    owner: [0; 32],
                    code_hash: [0; 32],
                },
            )
        })
        .unwrap();

        let mut balances = Vec::new();
        env.view(|r| {
            r.for_each::<Accounts>(|_, v| {
                balances.push(v.balance);
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(balances, vec![1, 2, 3]);
    }

    // --- Refusals: loud, never silent ----------------------------------------

    #[test]
    fn a_newer_version_is_refused_with_a_typed_error() {
        let (_dir, env) = open();

        // What a future binary would have written.
        env.update(|w| w.put::<AccountsRaw>(&KEY, &vec![9, 0, 0, 0]))
            .unwrap();

        let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::Codec(CodecError::NewerVersion {
                    tag: 9,
                    current: 3,
                    ..
                })
            ),
            "expected a newer-version refusal, got {err:?}"
        );
    }

    #[test]
    fn an_empty_value_reports_a_missing_tag() {
        let (_dir, env) = open();
        env.update(|w| w.put::<AccountsRaw>(&KEY, &Vec::new()))
            .unwrap();

        let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
        assert!(
            matches!(err, DbError::Codec(CodecError::MissingVersionTag { .. })),
            "expected a missing-tag error, got {err:?}"
        );
    }

    #[test]
    fn a_known_tag_with_wrong_bytes_fails_rather_than_misreading() {
        let (_dir, env) = open();
        // Tagged v2, but only a v1-sized payload behind it.
        let mut bytes = vec![2];
        bytes.extend_from_slice(&borsh::to_vec(&AccountV1 { balance: 5 }).unwrap());
        env.update(|w| w.put::<AccountsRaw>(&KEY, &bytes)).unwrap();

        let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
        assert!(
            matches!(err, DbError::Codec(CodecError::Decode { .. })),
            "expected a decode error, got {err:?}"
        );
    }

    #[test]
    fn a_detached_context_refuses_a_table_read() {
        let ctx = UpgradeCtx::detached();
        let bytes = tagged(
            2,
            &AccountV2 {
                balance: 1,
                nonce: 1,
                owner: OWNER,
            },
        );

        let err = <Accounts as VersionedTable>::decode_tagged(&bytes, &ctx).unwrap_err();
        assert!(
            matches!(err, CodecError::NoUpgradeContext { .. }),
            "expected a no-context error, got {err:?}"
        );

        // A converter that only defaults still works without a transaction.
        let v1 = tagged(1, &AccountV1 { balance: 4 });
        let err = <Accounts as VersionedTable>::decode_tagged(&v1, &ctx).unwrap_err();
        assert!(
            matches!(err, CodecError::NoUpgradeContext { .. }),
            "the v1 value folds through v2 -> v3, which does read a table: {err:?}"
        );
    }

    #[test]
    fn an_up_converter_read_cycle_is_refused() {
        let (_dir, env) = open();

        // A v1 value whose up-converter reads the very key being decoded: each
        // decode nests one level deeper instead of terminating.
        env.update(|w| w.put::<LoopiesRaw>(&KEY, &tagged(1, &LoopyV1 { key: KEY })))
            .unwrap();

        let err = env.view(|r| r.get::<Loopies>(&KEY)).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::Codec(CodecError::UpgradeContextDepth { depth, .. })
                    if depth == MAX_UPGRADE_DEPTH
            ),
            "expected a depth refusal, got {err:?}"
        );
    }

    /// A raw view of the `Loopies` sub-database.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct LoopiesRaw;

    impl Schema for LoopiesRaw {
        const NAME: &'static str = "Loopies";
        type Key = Hash;
        type Value = Vec<u8>;
    }
    impl_borsh_key_codec!(LoopiesRaw, Hash);
    impl_raw_value_codec!(LoopiesRaw);

    // --- Golden fixtures ------------------------------------------------------

    #[test]
    fn golden_fixtures_replay_every_shipped_version() {
        let (_dir, env) = open();
        env.update(|w| w.put::<Codes>(&OWNER, &CODE)).unwrap();

        let v1 = tagged(1, &AccountV1 { balance: 1 });
        let v2 = tagged(
            2,
            &AccountV2 {
                balance: 2,
                nonce: 2,
                owner: OWNER,
            },
        );
        let v3 = tagged(
            3,
            &AccountV3 {
                balance: 3,
                nonce: 3,
                owner: OWNER,
                code_hash: CODE,
            },
        );
        let fixtures = [
            GoldenFixture::new(1, &v1),
            GoldenFixture::new(2, &v2),
            GoldenFixture::new(3, &v3),
        ];

        let decoded: Vec<AccountV3> = env
            .view::<_, DbError>(|r| {
                Ok(check_fixtures::<Accounts>(&fixtures, &r.upgrade_ctx()).unwrap())
            })
            .unwrap();

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].balance, 1);
        assert_eq!(decoded[1].code_hash, CODE, "v2 folds through its converter");
        assert_eq!(decoded[2].nonce, 3);
    }

    #[test]
    fn a_missing_fixture_fails_the_check() {
        let v1 = tagged(1, &AccountV1 { balance: 1 });
        let err =
            check_fixtures::<Accounts>(&[GoldenFixture::new(1, &v1)], &UpgradeCtx::detached())
                .unwrap_err();

        assert!(
            matches!(err, FixtureError::MissingVersion { version: 2, .. }),
            "expected a missing-version report, got {err:?}"
        );
    }

    #[test]
    fn a_mislabelled_fixture_fails_the_check() {
        let v1 = tagged(1, &AccountV1 { balance: 1 });
        let v2 = tagged(
            2,
            &AccountV2 {
                balance: 2,
                nonce: 2,
                owner: OWNER,
            },
        );
        // The third fixture claims v3 but carries v1 bytes.
        let fixtures = [
            GoldenFixture::new(1, &v1),
            GoldenFixture::new(2, &v2),
            GoldenFixture::new(3, &v1),
        ];

        let err = check_fixtures::<Accounts>(&fixtures, &UpgradeCtx::detached()).unwrap_err();
        assert!(
            matches!(
                err,
                FixtureError::TagMismatch {
                    version: 3,
                    tag: 1,
                    ..
                }
            ),
            "expected a tag mismatch, got {err:?}"
        );
    }

    // --- Declared metadata ----------------------------------------------------

    #[test]
    fn the_table_reports_its_version_chain() {
        assert_eq!(<Accounts as Schema>::NAME, "Accounts");
        assert_eq!(<Accounts as VersionedTable>::CURRENT_VERSION, 3);
        assert_eq!(<Accounts as VersionedTable>::VERSIONS, &[1, 2, 3]);
    }

    #[test]
    fn a_round_trip_through_the_store_is_stable() {
        let (_dir, env) = open();
        let account = AccountV3 {
            balance: 42,
            nonce: 7,
            owner: OWNER,
            code_hash: CODE,
        };
        env.update(|w| w.put::<Accounts>(&KEY, &account)).unwrap();
        assert_eq!(
            env.view(|r| r.get::<Accounts>(&KEY)).unwrap(),
            Some(account)
        );
    }

    #[test]
    fn a_hand_written_version_codec_round_trips_under_its_tag() {
        let (_dir, env) = open();
        let tally = TallyV1 {
            bytes: vec![1, 2, 3],
        };
        env.update(|w| w.put::<Tallies>(&KEY, &tally)).unwrap();

        assert_eq!(
            env.view(|r| r.get::<Tallies>(&KEY)).unwrap(),
            Some(tally.clone())
        );

        // The family tags the hand-written payload exactly as it tags a generated
        // one: a leading version byte, then whatever the codec produced.
        let mut expected = vec![1u8];
        expected.extend_from_slice(&tally.encode_payload().unwrap());
        assert_eq!(
            env.view(|r| r.get::<TalliesRaw>(&KEY)).unwrap(),
            Some(expected)
        );
    }
}
