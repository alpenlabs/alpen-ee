//! Per-block state diffs: written by the state-diff exex as blocks execute,
//! read by the DA pipeline to build blobs and by the provers to size them.

use alloy_primitives::B256;
use alpen_reth_statediff::BlockStateChanges;
use strata_db_types::DbResult;

/// Writes and removes a block's state diff.
pub trait StateDiffStore {
    /// Stores `state_diff` under the block's hash and number.
    fn put_state_diff(
        &self,
        block_hash: B256,
        block_number: u64,
        state_diff: &BlockStateChanges,
    ) -> DbResult<()>;

    /// Removes the diff stored for `block_hash`, if any.
    fn del_state_diff(&self, block_hash: B256) -> DbResult<()>;
}

/// Reads a block's state diff by hash or by number.
pub trait StateDiffProvider: Send + Sync {
    /// The diff stored for `block_hash`, if any.
    fn get_state_diff_by_hash(&self, block_hash: B256) -> DbResult<Option<BlockStateChanges>>;

    /// The diff stored for the block at `block_number`, if any.
    fn get_state_diff_by_number(&self, block_number: u64) -> DbResult<Option<BlockStateChanges>>;
}
