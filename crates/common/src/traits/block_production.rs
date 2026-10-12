use crate::ExecBlockRecord;

/// Decides whether the sequencer builds its next block.
pub trait BlockProductionControl: Send + Sync {
    /// Returns whether a new block should be built on top of `parent`.
    fn should_build_next_block(&self, parent: &ExecBlockRecord) -> bool;
}
