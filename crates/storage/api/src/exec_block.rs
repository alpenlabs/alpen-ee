use alpen_common::{ExecBlockPayload, ExecBlockRecord};
use async_trait::async_trait;
use strata_acct_types::Hash;

use crate::StorageError;

#[cfg_attr(feature = "test-utils", mockall::automock)]
#[async_trait]
/// Persistence for Exec Blocks
///
/// This expects blocks to be stored as "finalized" or "unfinalized".
/// "finalized" blocks should only ever have a single canonical chain.
/// "unfinalized" blocks may have forks, and all such blocks need to be persisted.
pub trait ExecBlockStorage: Send + Sync {
    /// Save block data and payload for a given block hash.
    ///
    /// Blocks are uniquely identified by `ExecBlockRecord::blockhash()`. If a block with the same
    /// hash already exists, the original data is preserved (no overwrite). Succeeds in both cases.
    async fn save_exec_block(
        &self,
        block: ExecBlockRecord,
        payload: ExecBlockPayload,
    ) -> Result<(), StorageError>;

    /// Initialize the finalized chain with a genesis block.
    ///
    /// The block must have been previously saved via `save_exec_block`. Should be idempotent:
    /// calling multiple times with the same hash should succeed. Fails if block doesn't exist.
    async fn init_finalized_chain(&self, hash: Hash) -> Result<(), StorageError>;

    /// Extend the finalized chain up to and including `new_tip`.
    ///
    /// Walks parent links from `new_tip` back to the current best finalized block and inserts
    /// each missing canonical block in order. Atomic: either all intermediate finalized entries
    /// are inserted or none are. Works for both single-block and multi-block extensions.
    /// Fails if chain is empty, `new_tip` is unknown, an intermediate parent is missing, or
    /// `new_tip` does not descend from the current finalized tip.
    async fn extend_finalized_chain(&self, new_tip: Hash) -> Result<(), StorageError>;

    /// Revert the finalized chain to a specified height.
    ///
    /// All blocks above `to_height` are removed from the finalized chain and become unfinalized.
    /// Block data is preserved. Fails if chain is empty.
    async fn revert_finalized_chain(&self, to_height: u64) -> Result<(), StorageError>;

    /// Permanently delete all block data and payloads below the specified height.
    ///
    /// Removes blocks and their payloads at heights < `to_height`. Block data cannot be recovered
    /// after pruning.
    async fn prune_block_data(&self, to_height: u64) -> Result<(), StorageError>;

    /// Get the highest block in the finalized chain.
    ///
    /// Returns `None` if the finalized chain is empty (not yet initialized). Returns the block
    /// with the highest block number that has been finalized.
    async fn best_finalized_block(&self) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get the finalized block at a specific height.
    ///
    /// Returns `None` if no block is finalized at the given height. Returns the block
    /// that is finalized at the specified height.
    async fn get_finalized_block_at_height(
        &self,
        height: u64,
    ) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get the finalized height of a block, if it exists on the finalized chain.
    ///
    /// Returns `None` if the block doesn't exist, hasn't been finalized, or exists at a height
    /// but a different block hash is finalized at that height (fork case).
    async fn get_finalized_height(&self, hash: Hash) -> Result<Option<u64>, StorageError>;

    /// Get all unfinalized blocks (height > best finalized height).
    ///
    /// Returns block hashes ordered by incrementing height. May include multiple blocks at the
    /// same height (forks). Fails if finalized chain is empty.
    async fn get_unfinalized_blocks(&self) -> Result<Vec<Hash>, StorageError>;

    /// Get block data for a block by its hash.
    ///
    /// Returns `None` if the block doesn't exist. Works for both finalized and unfinalized blocks.
    async fn get_exec_block(&self, hash: Hash) -> Result<Option<ExecBlockRecord>, StorageError>;

    /// Get block payload for a block by its hash.
    ///
    /// Returns `None` if the block doesn't exist. Works for both finalized and unfinalized blocks.
    async fn get_block_payload(&self, hash: Hash)
        -> Result<Option<ExecBlockPayload>, StorageError>;

    /// Delete a single block and its payload by hash.
    ///
    /// Removes the block data, payload, and updates the height index. Returns an error if the
    /// block is in the finalized chain. Returns `Ok(())` if the block doesn't exist (idempotent).
    async fn delete_exec_block(&self, hash: Hash) -> Result<(), StorageError>;
}
