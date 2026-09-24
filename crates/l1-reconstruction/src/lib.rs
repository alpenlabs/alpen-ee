//! Builds ordered EVM replay batches from decoded EE DA blobs.
//!
//! The crate sorts blobs by update sequence number, rejects duplicates and gaps,
//! and converts them into source-neutral replay batches.

mod ordering;

pub use ordering::{build_ordered_replay_batches, BatchSequenceError};
