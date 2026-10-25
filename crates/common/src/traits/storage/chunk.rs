//! Storage for Chunks and batch-chunk associations.
//!
//! Chunks are subsets of blocks in a batch, and units for proving EE chain
//! execution, typically limited by prover constraints.
//!
//! A batch will consist of 1 or more chunks. These chunks must fit inside
//! batch boundaries.
//!
//! ```text
//! ... | ------------ Batch 1 ------------- | --------- Batch 2 ------- | ...
//! ... | --- Chunk 1 ---- | --- Chunk 2 --- | -------- Chunk 3 -------- | ...
//! ```

use async_trait::async_trait;

use crate::{BatchId, Chunk, ChunkId, ChunkStatus, StorageError};

/// Storage for Chunks and batch-chunk associations.
///
/// [`ChunkId`] is deterministic, based on ids of blocks covered, and so is
/// unique across forks and reorgs.
#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
pub trait ChunkStorage: Send + Sync {
    /// Save the next chunk.
    ///
    /// The entry must extend the last chunk present in storage.
    async fn save_next_chunk(&self, chunk: Chunk) -> Result<(), StorageError>;

    /// Update an existing chunk's status.
    async fn update_chunk_status(
        &self,
        chunk_id: ChunkId,
        status: ChunkStatus,
    ) -> Result<(), StorageError>;

    /// Remove all chunks where idx >= from_idx.
    async fn revert_chunks_from(&self, from_idx: u64) -> Result<(), StorageError>;

    /// Get a chunk by its id, if it exists.
    async fn get_chunk_by_id(
        &self,
        chunk_id: ChunkId,
    ) -> Result<Option<(Chunk, ChunkStatus)>, StorageError>;

    /// Get a chunk by its idx, if it exists.
    async fn get_chunk_by_idx(
        &self,
        idx: u64,
    ) -> Result<Option<(Chunk, ChunkStatus)>, StorageError>;

    /// Get the chunk with the highest idx, if it exists.
    async fn get_latest_chunk(&self) -> Result<Option<(Chunk, ChunkStatus)>, StorageError>;

    /// Set or update batch-chunk association.
    async fn set_batch_chunks(
        &self,
        batch_id: BatchId,
        chunks: Vec<ChunkId>,
    ) -> Result<(), StorageError>;

    /// Get the chunk-id list previously set for a batch.
    ///
    /// Returns `None` if [`set_batch_chunks`](ChunkStorage::set_batch_chunks)
    /// has never been called for `batch_id`. Used by the prover to fan out
    /// chunk-proof tasks.
    async fn get_batch_chunks(
        &self,
        batch_id: BatchId,
    ) -> Result<Option<Vec<ChunkId>>, StorageError>;
}
