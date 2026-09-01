//! Per-value schema versioning: version-dispatch on read, current format on write.
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
//!   -> decode with the decoder registered for (Family, t)   // -> concrete V_t
//!   -> up-convert chain V_t -> V_{t+1} -> ... -> V_current  // -> current type
//! ```
//!
//! Writes always emit the current version with the current tag. Reads never
//! write back, so a read stays a read: cold keys keep their old format on disk
//! until the application naturally writes them again (see [`Regime`]).
//!
//! # Declaring a versioned value
//!
//! A *family* is the stable handle for one stored value across all its
//! versions: a marker type that [`versioned_value!`](crate::versioned_value)
//! generates, naming its versions ascending, the last one current. Each shipped
//! version is an ordinary struct with a [`SchemaVersion`] impl; the converters
//! between consecutive versions are ordinary [`UpConvert`] impls.
//!
//! ```
//! use alpen_db_store_mdbx::{
//!     impl_schema_version_borsh, versioned_value, CodecError, UpConvert, UpgradeCtx,
//! };
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
//! impl_schema_version_borsh!(AccountState, AccountStateV1, 1);
//! impl_schema_version_borsh!(AccountState, AccountStateV2, 2);
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
//! versioned_value! {
//!     /// EE account state.
//!     pub AccountState {
//!         1 => AccountStateV1,
//!         2 => AccountStateV2,
//!     }
//! }
//! ```
//!
//! Bumping the version means *adding* a struct and *adding* one converter.
//! Shipped structs and converters are never edited: values carrying their tags
//! are still on disk. A missing `N -> N+1` converter is a compile error, because
//! the generated chain calls [`UpConvert`] for each consecutive pair.
//!
//! The family is a type of its own rather than another name for the current
//! version, so a version may be a type this crate does not own — a store's
//! records frequently are — and so tooling has one handle per stored format
//! regardless of how many tables share it.
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

/// One shipped on-disk version of the value family `F`.
///
/// Implementations are attached by the `impl_schema_version_*` macros. Once a
/// version has shipped, neither its struct nor its codec may change: bytes
/// carrying its tag are still on disk.
///
/// The family parameter is what lets a version be a type the declaring crate
/// does not own: the family marker is always local to it.
pub trait SchemaVersion<F>: Sized {
    /// The stable name of the value family this is a version of.
    const FAMILY: &'static str;

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

/// Folds a shipped version of family `F` up to `F`'s current type.
///
/// Generated by [`versioned_value!`](crate::versioned_value) by chaining
/// [`UpConvert`] across every consecutive pair, so a missing edge fails the
/// build rather than a decode.
pub trait LiftToCurrent<F, Current>: Sized {
    /// Runs the up-convert chain from `self` to the current version.
    fn lift_to_current(self, ctx: &UpgradeCtx<'_>) -> Result<Current, CodecError>;
}

/// A family of stored formats: a version tag followed by that version's payload.
///
/// Implemented on the family marker by
/// [`versioned_value!`](crate::versioned_value); [`Self::Value`] is whichever
/// version is current.
pub trait VersionedValue {
    /// The type the family currently decodes to and writes.
    type Value;

    /// The stable name of the value family.
    const FAMILY: &'static str;

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
/// Public because [`versioned_value!`](crate::versioned_value) expands into it.
pub fn split_version_tag<'a>(
    family: &'static str,
    bytes: &'a [u8],
) -> Result<(u8, &'a [u8]), CodecError> {
    match bytes.split_first() {
        Some((tag, payload)) => Ok((*tag, payload)),
        None => Err(CodecError::MissingVersionTag { schema: family }),
    }
}

/// Builds the error for a tag no decoder claims, distinguishing "written by a
/// newer binary" from a gap in the chain.
///
/// Public because [`versioned_value!`](crate::versioned_value) expands into it.
pub fn unknown_version_error(family: &'static str, tag: u8, current: u8) -> CodecError {
    if tag > current {
        CodecError::NewerVersion {
            schema: family,
            tag,
            current,
        }
    } else {
        CodecError::UnknownVersion {
            schema: family,
            tag,
        }
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

    use super::{UpgradeCtx, VersionedValue};
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
        #[error("`{family}`: no golden fixture for version {version}")]
        MissingVersion {
            /// The value family.
            family: &'static str,
            /// The version with no fixture.
            version: u8,
        },

        /// A fixture claims a version the family does not declare.
        #[error("`{family}`: golden fixture claims unknown version {version}")]
        UnknownVersion {
            /// The value family.
            family: &'static str,
            /// The version the fixture claims.
            version: u8,
        },

        /// A fixture's declared version disagrees with its leading tag.
        #[error("`{family}`: golden fixture for version {version} is tagged {tag}")]
        TagMismatch {
            /// The value family.
            family: &'static str,
            /// The version the fixture claims.
            version: u8,
            /// The tag actually found in the bytes.
            tag: u8,
        },

        /// A fixture failed to decode and fold up to the current version.
        #[error("`{family}`: golden fixture for version {version} failed to decode")]
        Decode {
            /// The value family.
            family: &'static str,
            /// The version that failed.
            version: u8,
            /// The decode failure.
            #[source]
            source: CodecError,
        },
    }

    /// Checks that `fixtures` cover every version of family `F` exactly once,
    /// and that each one decodes and folds up to the current version.
    ///
    /// Returns the decoded values, in fixture order, so a caller can assert on
    /// the up-converted contents too.
    ///
    /// `ctx` is the context the up-converters run in. Pass
    /// [`UpgradeCtx::detached`] for a family whose converters only default or
    /// derive from `self`; a family that reads other tables needs a fixture
    /// store populated in a transaction.
    pub fn check_fixtures<F: VersionedValue>(
        fixtures: &[GoldenFixture<'_>],
        ctx: &UpgradeCtx<'_>,
    ) -> Result<Vec<F::Value>, FixtureError> {
        let family = F::FAMILY;

        for fixture in fixtures {
            if !F::VERSIONS.contains(&fixture.version) {
                return Err(FixtureError::UnknownVersion {
                    family,
                    version: fixture.version,
                });
            }
        }

        for version in F::VERSIONS {
            if !fixtures.iter().any(|f| f.version == *version) {
                return Err(FixtureError::MissingVersion {
                    family,
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
                        family,
                        version: fixture.version,
                        tag: *tag,
                    })
                }
                None => {
                    return Err(FixtureError::Decode {
                        family,
                        version: fixture.version,
                        source: CodecError::MissingVersionTag { schema: family },
                    })
                }
            }
        }

        fixtures
            .iter()
            .map(|fixture| {
                F::decode_tagged(fixture.bytes, ctx).map_err(|source| FixtureError::Decode {
                    family,
                    version: fixture.version,
                    source,
                })
            })
            .collect()
    }
}
