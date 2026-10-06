//! What the sequencer has already published to DA, so later batches can
//! leave it out.

use alloy_primitives::B256;
use strata_db_types::DbResult;

/// DA filter that grows as batches reach `DaComplete` status.
///
/// Tracks which data items have already been published to DA so that future
/// batches can omit them. Currently tracks deployed contract bytecodes;
/// extensible for address dedup and other filtering logic.
pub trait DaContext {
    /// Returns `true` if the bytecode identified by `code_hash` was included
    /// in a previously confirmed batch's DA.
    fn is_code_hash_published(&self, code_hash: &B256) -> DbResult<bool>;

    /// Marks the given code hashes as published. Idempotent.
    fn mark_code_hashes_published(&self, code_hashes: &[B256]) -> DbResult<()>;

    /// Updates the DA filter with data from the given blocks.
    ///
    /// Reads state diffs for each block and records which data items have been
    /// published to DA. Currently tracks deployed bytecodes; extensible for
    /// address dedup and other filtering logic.
    fn update_da_filter(&self, block_hashes: &[B256]) -> DbResult<()>;
}
