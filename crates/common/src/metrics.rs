//! Metric names more than one crate records under.

/// Height of the chain at each `status`: `head` (last block the sequencer
/// built), `preconf` (latest head sent to reth), `safe` and `finalized` (the
/// OL's confirmed and finalized EE blocks, when reth knows them). The
/// sequencer and the reth engine each record their own statuses.
pub const BLOCK_LATEST_HEIGHT: &str = "alpen_ee_block_latest_height";
