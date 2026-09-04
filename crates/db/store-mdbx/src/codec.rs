//! Typed table schema and key/value codec traits.
//!
//! These establish the encoding conventions (borsh, `strata-codec`, big-endian
//! integer keys) for MDBX-backed tables. Values decode to owned types (copy-out of the
//! transaction), which sidesteps the zero-copy lifetime hazards of holding a
//! borrowed view across a transaction boundary.
//!
//! Decoding takes an [`UpgradeCtx`] so that a value's decoder can be
//! version-dispatching (see the [`version`](crate::version) module): the
//! context lets an up-converter read other tables through the same dispatching
//! accessor, inside the ambient transaction. Codecs that do not version their
//! values simply ignore it.

use std::error::Error;

use crate::version::UpgradeCtx;

/// Boxed, thread-safe error used as the source of a [`CodecError`].
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Failure to encode or decode a key or value for a [`Schema`].
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Encoding a key or value failed.
    #[error("failed to encode value for table `{schema}`")]
    Encode {
        /// The table whose codec failed.
        schema: &'static str,
        /// The underlying encoder error.
        #[source]
        source: BoxError,
    },

    /// Decoding a key or value failed.
    #[error("failed to decode value for table `{schema}`")]
    Decode {
        /// The table whose codec failed.
        schema: &'static str,
        /// The underlying decoder error.
        #[source]
        source: BoxError,
    },

    /// A stored value was empty, so it carries no leading version tag.
    #[error("`{schema}`: stored value is empty, expected a leading version tag")]
    MissingVersionTag {
        /// The versioned value family (or table) whose value was empty.
        schema: &'static str,
    },

    /// The value carries a version tag newer than anything this binary knows,
    /// i.e. the store was written by a newer binary.
    ///
    /// Downgrading across a breaking change is a snapshot restore, not an online
    /// path; this refusal is what keeps it from being a silent misdecode.
    #[error(
        "`{schema}`: stored value is version {tag} but this binary only knows up to {current} \
         (store written by a newer binary)"
    )]
    NewerVersion {
        /// The versioned value family.
        schema: &'static str,
        /// The tag found on disk.
        tag: u8,
        /// The newest version this binary can decode.
        current: u8,
    },

    /// The value carries a tag at or below the current version that no decoder
    /// claims — a gap in the version chain, which must never happen.
    #[error("`{schema}`: stored value has version {tag}, for which no decoder is registered")]
    UnknownVersion {
        /// The versioned value family.
        schema: &'static str,
        /// The tag found on disk.
        tag: u8,
    },

    /// An up-converter between two shipped versions failed.
    #[error("`{schema}`: up-converting a value from v{from} to v{to} failed")]
    Upgrade {
        /// The versioned value family.
        schema: &'static str,
        /// The version converted from.
        from: u8,
        /// The version converted to.
        to: u8,
        /// The underlying converter error.
        #[source]
        source: BoxError,
    },

    /// An up-converter tried to read another table, but the value was decoded
    /// outside a transaction (a detached [`UpgradeCtx`]).
    #[error("`{schema}`: up-converter needs to read other tables, but no transaction is in scope")]
    NoUpgradeContext {
        /// The table the up-converter tried to read.
        schema: &'static str,
    },

    /// Up-converter context reads nested past the depth limit, which almost
    /// always means the up-converters form a read cycle.
    #[error("`{schema}`: up-converter context reads nested {depth} deep (probable read cycle)")]
    UpgradeContextDepth {
        /// The table the up-converter tried to read.
        schema: &'static str,
        /// The depth reached.
        depth: u8,
    },

    /// Reading another table from an up-converter's context failed.
    #[error("`{schema}`: up-converter context read failed")]
    UpgradeContextRead {
        /// The table the up-converter tried to read.
        schema: &'static str,
        /// The underlying store error.
        #[source]
        source: BoxError,
    },
}

impl CodecError {
    /// Constructs an [`CodecError::Encode`] for `schema` from any boxable error.
    pub fn encode(schema: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Encode {
            schema,
            source: source.into(),
        }
    }

    /// Constructs an [`CodecError::Decode`] for `schema` from any boxable error.
    pub fn decode(schema: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Decode {
            schema,
            source: source.into(),
        }
    }

    /// Constructs an [`CodecError::Upgrade`] for the `from -> to` edge of
    /// `schema` from any boxable error.
    pub fn upgrade(schema: &'static str, from: u8, to: u8, source: impl Into<BoxError>) -> Self {
        Self::Upgrade {
            schema,
            from,
            to,
            source: source.into(),
        }
    }
}

/// A typed MDBX table: a named sub-database with a key and value type.
///
/// Implementers are usually generated by the [`define_table!`](crate::define_table)
/// family of macros. [`Schema::NAME`] is the MDBX sub-database name and must be
/// unique within an environment.
pub trait Schema: Sized + Send + Sync + 'static {
    /// The MDBX sub-database name. Must be unique within an environment.
    const NAME: &'static str;

    /// Whether the table stores multiple values per key (MDBX `DUP_SORT`).
    const DUP_SORT: bool = false;

    /// The key type; encoded via its [`KeyCodec`] impl.
    type Key: KeyCodec<Self>;

    /// The value type; encoded via its [`ValueCodec`] impl.
    type Value: ValueCodec<Self>;
}

/// Encodes and decodes a [`Schema`]'s key to and from bytes.
///
/// Key encoding determines cursor order (MDBX orders keys lexicographically by
/// their bytes), so integer keys that must sort numerically should use a
/// big-endian codec.
pub trait KeyCodec<S: Schema>: Sized {
    /// Encodes the key to its on-disk byte representation.
    fn encode_key(&self) -> Result<Vec<u8>, CodecError>;

    /// Decodes the key from its on-disk byte representation.
    fn decode_key(bytes: &[u8]) -> Result<Self, CodecError>;
}

/// Encodes and decodes a [`Schema`]'s value to and from bytes.
///
/// Writes always emit the current format. Reads accept every format this binary
/// knows: a versioned codec dispatches on the value's leading tag and folds it
/// up to the current type (see [`version`](crate::version)).
pub trait ValueCodec<S: Schema>: Sized {
    /// Encodes the value to its on-disk byte representation, in the current
    /// format.
    fn encode_value(&self) -> Result<Vec<u8>, CodecError>;

    /// Decodes the value from its on-disk byte representation.
    ///
    /// `ctx` is the ambient transaction, for version-dispatching codecs whose
    /// up-converters derive a new field from other tables. Codecs that do not
    /// version their values ignore it.
    fn decode_value(bytes: &[u8], ctx: &UpgradeCtx<'_>) -> Result<Self, CodecError>;
}
