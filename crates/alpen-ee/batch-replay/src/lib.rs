//! Pure primitives for replaying ordered EE batches against Ethereum state.
//!
//! This crate operates on source-neutral replay batches. It does not fetch L1
//! data, parse DA envelopes, access storage, or run services.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
mod replay;
mod snapshot;

pub use error::BatchReplayError;
pub use replay::{
    replay_from_genesis, replay_from_snapshot, AppliedBatchRange, AppliedBatchRoot,
    BatchReplayOutcome, EvmReplayBatch,
};
pub use snapshot::BatchReplaySnapshot;
