//! Pure replay primitives for applying EE replay batches to Ethereum state.
//!
//! This crate operates on source-neutral replay batches. It does not fetch L1
//! data, parse DA envelopes, access storage, or run services.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
mod replay;
mod snapshot;

pub use error::ReplayError;
pub use replay::{
    replay_from_genesis, replay_from_snapshot, AppliedBatchRoot, AppliedRange, EvmReplayBatch,
    ReplayOutcome,
};
pub use snapshot::ReplayStateSnapshot;
