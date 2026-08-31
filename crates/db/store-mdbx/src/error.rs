//! Error types for the MDBX store.

use signet_libmdbx::{MdbxError, ReadError};

use crate::codec::CodecError;

/// Convenience result alias for MDBX store operations.
pub type DbResult<T> = Result<T, DbError>;

/// An error from the MDBX store: the engine, a codec, or environment setup.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// An error returned by the MDBX engine.
    #[error("mdbx: {0}")]
    Mdbx(#[from] MdbxError),

    /// An error returned on the read path: an engine error or a value-decode
    /// failure surfaced by `signet-libmdbx`'s [`ReadError`].
    #[error("mdbx read: {0}")]
    Read(#[from] ReadError),

    /// A key or value codec failed.
    #[error(transparent)]
    Codec(#[from] CodecError),

    /// A write tried to change an existing value on a table declared
    /// [`Regime::Immutable`](crate::Regime).
    ///
    /// Such a table's values are fixed once written: every format it has ever
    /// held stays valid forever and existing values are frozen. A key may be
    /// inserted, re-put byte-identically, or deleted — never rewritten with
    /// different content.
    #[error("table `{schema}` is immutable: an existing value cannot be rewritten")]
    ImmutableOverwrite {
        /// The table that refused the write.
        schema: &'static str,
    },

    /// Environment setup failed (e.g. creating the data directory).
    #[error("mdbx environment: {0}")]
    Env(String),
}
