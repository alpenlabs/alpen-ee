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

    /// Environment setup failed (e.g. creating the data directory).
    #[error("mdbx environment: {0}")]
    Env(String),
}

impl DbError {
    /// Returns whether retrying the operation may succeed without reopening the environment.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Mdbx(error) | Self::Read(ReadError::Mdbx(error)) => {
                is_transient_engine_error(error)
            }
            Self::Read(ReadError::Decoding(_)) | Self::Codec(_) | Self::Env(_) => false,
        }
    }
}

fn is_transient_engine_error(error: &MdbxError) -> bool {
    matches!(
        error,
        MdbxError::ReadersFull
            | MdbxError::Busy
            | MdbxError::ReadTransactionTimeout
            | MdbxError::SnapshotDivergence
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retryable_mdbx_conditions_are_transient() {
        for error in [
            MdbxError::ReadersFull,
            MdbxError::Busy,
            MdbxError::ReadTransactionTimeout,
            MdbxError::SnapshotDivergence,
        ] {
            assert!(DbError::Mdbx(error).is_transient());
        }
    }

    #[test]
    fn test_read_error_preserves_mdbx_classification() {
        assert!(DbError::Read(ReadError::Mdbx(MdbxError::Busy)).is_transient());
        assert!(!DbError::Read(ReadError::Mdbx(MdbxError::Corrupted)).is_transient());
    }

    #[test]
    fn test_errors_requiring_intervention_are_not_transient() {
        for error in [
            MdbxError::BadRslot,
            MdbxError::MapFull,
            MdbxError::UnableExtendMapSize,
            MdbxError::Corrupted,
        ] {
            assert!(!DbError::Mdbx(error).is_transient());
        }
    }
}
