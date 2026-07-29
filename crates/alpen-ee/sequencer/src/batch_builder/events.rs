//! Events emitted by the batch builder for downstream consumers.
//!
//! The chunk builder consumes these events via an mpsc channel to track
//! block processing and batch sealing without independently watching
//! `preconf_rx`. This guarantees the chunk builder never runs ahead of
//! the batch builder and inherits reorg handling for free.
//!
//! Each event carries exactly one fact (a block was admitted, or a batch
//! was sealed), emitted at the moment that fact becomes true. A block can
//! be preceded and/or followed by a batch seal (a threshold seal right
//! before it, a predicate-rotation seal right after) — the mpsc channel's
//! FIFO ordering conveys that relationship, so there's no need to bundle
//! them into one struct with positional fields.

use alpen_ee_common::{BatchId, BlockNumHash};

/// Event emitted by the batch builder after processing a block or
/// handling a reorg.
///
/// Sent on a bounded [`tokio::sync::mpsc`] channel. The chunk builder
/// is the sole consumer. `batch_builder_task` is a single producer, so
/// emission order equals reception order.
#[derive(Debug, Clone)]
pub enum BatchBuilderEvent {
    /// A block was admitted to the open batch accumulator.
    BlockAdmitted {
        /// The block that was just accumulated.
        block: BlockNumHash,
        /// Index of the batch this block belongs to. The chunk builder
        /// uses this to set `Chunk::batch_idx` and to validate that
        /// events arrive in the expected order.
        batch_idx: u64,
    },
    /// A batch was sealed. `batch_id` carries its own `last_block`, so
    /// this is self-describing regardless of what comes before or after
    /// it in the event stream. The chunk builder must force-seal its
    /// current chunk at this boundary and call
    /// [`ChunkStorage::set_batch_chunks`](alpen_ee_common::ChunkStorage::set_batch_chunks).
    BatchSealed {
        /// The batch that was just sealed.
        batch_id: BatchId,
    },
    /// A reorg was handled by the batch builder. The chunk builder
    /// must revert to match.
    Reorg(ReorgEventData),
}

impl BatchBuilderEvent {
    pub fn block_admitted(block: BlockNumHash, batch_idx: u64) -> Self {
        Self::BlockAdmitted { block, batch_idx }
    }

    pub fn batch_sealed(batch_id: BatchId) -> Self {
        Self::BatchSealed { batch_id }
    }

    pub fn reorg(revert_to: BlockNumHash, last_valid_batch_idx: u64) -> Self {
        Self::Reorg(ReorgEventData {
            revert_to,
            last_valid_batch_idx,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReorgEventData {
    /// The new "last good" block. Corresponds to
    /// `state.prev_batch_end()` after the batch builder handled
    /// the reorg.
    pub revert_to: BlockNumHash,
    /// Index of the last canonical batch after the revert.
    pub last_valid_batch_idx: u64,
}
