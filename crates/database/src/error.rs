use alpen_common::{BatchId, ChunkId, StorageError};
use alpen_mdbx::DbError as StoreDbError;
use bitcoin::Txid;
use strata_acct_types::Hash;
use strata_codec::CodecError;
use strata_identifiers::OLBlockId;
use strata_storage_common::exec::OpsError;
use thiserror::Error;
use tokio::task::JoinError;

pub type DbResult<T> = Result<T, DbError>;

/// Database-specific errors.
#[derive(Debug, Clone, Error)]
pub enum DbError {
    /// Attempted to persist a null OL block.
    #[error("null OL block should not be persisted")]
    NullOLBlock,

    /// OL slot was skipped in sequential persistence.
    #[error("OL entries must be persisted sequentially but provided nonsequentially (exp next {expected}, got {got})")]
    SkippedOLSlot { expected: u64, got: u64 },

    /// Transaction conflict: slot is already filled.
    #[error("likely db txn conflict, OL slot {0} already filled")]
    TxnFilledOLSlot(u64),

    /// Transaction conflict: expected slot to be empty.
    #[error("likely db txn conflict, OL slot {0} should be empty")]
    TxnExpectEmptyOLSlot(u64),

    /// Account state is missing for the given block.
    #[error("account state missing (at blkid {0})")]
    MissingAccountState(OLBlockId),

    /// Finalized chain is empty.
    #[error("finalized exec block expected to be present")]
    FinalizedExecChainEmpty,

    /// Exec block is missing.
    #[error("missing expected exec blkid {0}")]
    MissingExecBlock(Hash),

    #[error("expected exec block finalized chain to be empty")]
    FinalizedExecChainGenesisBlockMismatch,

    #[error("provided blkid {0} does not extend chain")]
    ExecBlockDoesNotExtendChain(Hash),

    /// Walk from `new_tip` failed to reach the current finalized tip.
    ///
    /// This indicates one of:
    /// - `new_tip` is non-canonical (does not descend from finalized tip),
    /// - storage inconsistency (parent links / block numbers disagree), or
    /// - walk exceeded the expected height-difference budget without reaching the tip (e.g.
    ///   cyclic/corrupt parent links above finalized height).
    #[error("walk failed to reach finalized tip (new tip {new_tip}, finalized height {finalized_height})")]
    FinalizedWalkNotDescending {
        new_tip: Hash,
        finalized_height: u64,
    },

    /// Walk from `new_tip` did not reach finalized tip within expected height-difference budget.
    ///
    /// This usually indicates cyclic/corrupt parent links above finalized height, or severe
    /// block-number/parent inconsistency that prevented convergence.
    #[error("walk exhausted step budget before reaching tip (new tip {new_tip}, finalized height {finalized_height}, max steps {max_steps})")]
    FinalizedWalkStepBudgetExceeded {
        new_tip: Hash,
        finalized_height: u64,
        max_steps: u64,
    },

    #[error("likely db txn conflict, expected finalized height {0} to be empty")]
    TxnExpectEmptyFinalized(u64),

    #[error("likely db txn conflict, expected finalized height {0} to be {1}")]
    TxnExpectFinalized(u64, Hash),

    /// Attempted to delete a finalized block.
    #[error("tried to delete finalized block {0}")]
    CannotDeleteFinalizedBlock(Hash),

    /// Batch not found when trying to update status.
    #[error("batch {0} not found")]
    BatchNotFound(BatchId),

    /// Chunk not found when trying to update status.
    // NOTE: `ChunkId` doesn't implement `Display`, so use `Debug` here.
    #[error("chunk {0:?} not found")]
    ChunkNotFound(ChunkId),

    /// Batch deserialization error.
    #[error("Failed to deserialize batch: {0}")]
    BatchDeserialize(String),

    /// Database operation error.
    #[error("db ops: {0}")]
    DbOpsError(#[from] OpsError),

    /// A database worker task spawned on the blocking pool panicked or was
    /// cancelled before returning a result.
    #[error("db worker task failed to return a result: {0}")]
    WorkerPanic(String),

    /// MDBX database error (engine or codec).
    #[error("mdbx: {0}")]
    Mdbx(String),

    /// Other unspecified database error.
    #[error("{0}")]
    Other(String),
}

impl DbError {
    pub(crate) fn skipped_ol_slot(expected: u64, got: u64) -> DbError {
        DbError::SkippedOLSlot { expected, got }
    }
}

impl From<JoinError> for DbError {
    fn from(err: JoinError) -> Self {
        DbError::WorkerPanic(err.to_string())
    }
}

impl From<alpen_mdbx::DbError> for DbError {
    fn from(err: alpen_mdbx::DbError) -> Self {
        DbError::Mdbx(err.to_string())
    }
}

impl From<DbError> for StorageError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::SkippedOLSlot { expected, got } => StorageError::MissingSlot {
                attempted_slot: got,
                last_slot: expected,
            },
            DbError::CannotDeleteFinalizedBlock(hash) => {
                StorageError::CannotDeleteFinalizedBlock(format!("{:?}", hash))
            }
            e => StorageError::database(e.to_string()),
        }
    }
}

/// Failure to read or mutate the recovered EE DA database.
#[derive(Debug, Error)]
pub enum RecoveredDaDbError {
    /// A recovered DA blob conflicts with data stored under the same identity.
    #[error("recovered DA blob {update_seq_no}@{commit_txid} conflicts with the persisted blob")]
    BlobConflict {
        update_seq_no: u64,
        commit_txid: Txid,
    },

    /// Encoding or decoding a recovered DA blob failed.
    #[error("recovered DA blob codec failed: {0}")]
    BlobCodec(#[from] CodecError),

    /// The sequence number decoded from persisted bytes does not match the database key.
    #[error("recovered DA blob {expected} decodes to update sequence {actual}")]
    StoredUpdateSeqNoMismatch { expected: u64, actual: u64 },

    /// The persisted blob names a spec version this binary cannot decode.
    #[error("recovered DA blob has unsupported stored spec version {0}")]
    UnsupportedStoredSpecVersion(u16),

    /// MDBX or a table codec failed.
    #[error(transparent)]
    Mdbx(#[from] StoreDbError),

    /// A blocking database worker was cancelled before completing.
    #[error("recovered DA database worker was cancelled")]
    WorkerCancelled,

    /// A blocking database worker panicked.
    #[error("recovered DA database worker panicked: {0}")]
    WorkerPanicked(String),
}

impl RecoveredDaDbError {
    /// Returns whether retrying the operation may succeed without intervention.
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Mdbx(error) => error.is_transient(),
            Self::WorkerCancelled => true,
            Self::BlobConflict { .. }
            | Self::BlobCodec(_)
            | Self::StoredUpdateSeqNoMismatch { .. }
            | Self::UnsupportedStoredSpecVersion(_)
            | Self::WorkerPanicked(_) => false,
        }
    }
}

impl From<JoinError> for RecoveredDaDbError {
    fn from(error: JoinError) -> Self {
        if error.is_cancelled() {
            Self::WorkerCancelled
        } else {
            Self::WorkerPanicked(error.to_string())
        }
    }
}

/// Result type for recovered EE DA database operations.
pub type RecoveredDaDbResult<T> = Result<T, RecoveredDaDbError>;

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    #[tokio::test]
    async fn test_cancelled_worker_is_recoverable() {
        let task = tokio::spawn(pending::<()>());
        task.abort();
        let join_error = task.await.expect_err("aborted task must be cancelled");

        let error = RecoveredDaDbError::from(join_error);

        assert!(matches!(error, RecoveredDaDbError::WorkerCancelled));
        assert!(error.is_recoverable());
    }

    #[tokio::test]
    async fn test_panicked_worker_is_fatal() {
        let join_error = tokio::spawn(async { panic!("database worker panic") })
            .await
            .expect_err("panicked task must fail");

        let error = RecoveredDaDbError::from(join_error);

        assert!(matches!(error, RecoveredDaDbError::WorkerPanicked(_)));
        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_stored_sequence_mismatch_is_fatal() {
        assert!(!RecoveredDaDbError::StoredUpdateSeqNoMismatch {
            expected: 1,
            actual: 2,
        }
        .is_recoverable());
    }

    #[test]
    fn test_unsupported_stored_spec_version_is_fatal() {
        assert!(!RecoveredDaDbError::UnsupportedStoredSpecVersion(2).is_recoverable());
    }

    #[test]
    fn test_mdbx_recoverability_is_delegated() {
        let transient = StoreDbError::Mdbx(signet_libmdbx::MdbxError::Busy);
        let fatal = StoreDbError::Mdbx(signet_libmdbx::MdbxError::MapFull);

        assert!(RecoveredDaDbError::Mdbx(transient).is_recoverable());
        assert!(!RecoveredDaDbError::Mdbx(fatal).is_recoverable());
    }
}
