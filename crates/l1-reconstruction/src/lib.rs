//! Reconstructs EVM state from EE DA blobs recovered from L1.
//!
//! The crate sorts recovered blobs by update sequence number, rejects duplicates
//! and gaps, converts them into replay batches, and applies those batches to EVM
//! state.

mod ordering;
mod reconstruct;
#[cfg(test)]
mod test_utils;

pub use ordering::{build_ordered_replay_batches, BatchSequenceError};
pub use reconstruct::{
    reconstruct_from_genesis, reconstruct_from_snapshot, L1ReconstructionError,
    L1ReconstructionOutcome,
};
