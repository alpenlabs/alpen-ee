use alpen_common::{Batch, BatchId, BatchStatus};
use async_trait::async_trait;

use crate::StorageError;

/// Storage for Batches.
///
/// A batch is a contiguous group of blocks. One batch corresponds to one Account
/// Update Operation sent to OL, and batch size should typically be limited by DA
/// size constraints.
///
/// [`BatchId`] is deterministic, based on ids of blocks covered, and so is unique
/// across forks and reorgs.
#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
pub trait BatchStorage: Send + Sync {
    /// Save the genesis batch.
    ///
    /// If any batches exist in storage, this is a noop.
    async fn save_genesis_batch(&self, genesis_batch: Batch) -> Result<(), StorageError>;
    /// Save the next ee update.
    ///
    /// The entry must extend the last batch present in storage.
    async fn save_next_batch(&self, batch: Batch) -> Result<(), StorageError>;
    /// Update an existing ee update's status.
    async fn update_batch_status(
        &self,
        batch_id: BatchId,
        status: BatchStatus,
    ) -> Result<(), StorageError>;
    /// Remove all batches where idx > to_idx.
    async fn revert_batches(&self, to_idx: u64) -> Result<(), StorageError>;
    /// Get an ee update by its id, if it exists.
    async fn get_batch_by_id(
        &self,
        batch_id: BatchId,
    ) -> Result<Option<(Batch, BatchStatus)>, StorageError>;
    /// Get an ee update by its idx, if it exists.
    async fn get_batch_by_idx(
        &self,
        idx: u64,
    ) -> Result<Option<(Batch, BatchStatus)>, StorageError>;
    /// Get the ee update with the highest idx, if it exists.
    async fn get_latest_batch(&self) -> Result<Option<(Batch, BatchStatus)>, StorageError>;
}
