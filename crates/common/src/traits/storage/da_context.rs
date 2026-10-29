//! What the sequencer has already published to DA, so later batches can
//! leave it out.

use alloy_primitives::{Address, B256};
use strata_db_types::DbResult;

/// DA filter that grows as batches reach `DaComplete` status.
///
/// Tracks which data items have already been published to DA so that future
/// batches can omit them. Currently tracks deployed contract bytecodes;
/// extensible for address dedup and other filtering logic.
///
/// A bytecode is tracked by an account that took on that code. A later batch
/// may leave the bytecode out only if that account still holds it in the
/// batch's pre-state.
pub trait DaContext {
    /// Returns an account that held the bytecode identified by `code_hash` in
    /// a previously confirmed batch, if one is known.
    fn code_hash_holder(&self, code_hash: &B256) -> DbResult<Option<Address>>;

    /// Records an account holding each code hash. Keeps a holder already on
    /// record, so the answer for a code hash never changes once given.
    fn mark_code_hashes_published(&self, holders: &[(B256, Address)]) -> DbResult<()>;

    /// Updates the DA filter with data from the given blocks.
    ///
    /// Reads state diffs for each block and records which data items have been
    /// published to DA. Currently tracks deployed bytecodes; extensible for
    /// address dedup and other filtering logic.
    fn update_da_filter(&self, block_hashes: &[B256]) -> DbResult<()>;
}
