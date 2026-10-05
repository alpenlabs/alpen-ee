//! Admin-RPC-driven block production control.

use std::sync::atomic::{AtomicU64, Ordering};

use alpen_common::{BlockProductionControl, ExecBlockRecord};

/// Sentinel for "no limit".
const UNBOUNDED: u64 = u64::MAX;

/// [`BlockProductionControl`] driven by the admin RPC: stops production once
/// the tip reaches a configured block number.
///
/// Not a strong guarantee: a block already being built when the limit
/// changes still completes. State is in-memory only, so a restart clears it.
#[derive(Debug)]
pub struct RpcBlockProductionControl {
    /// Last block number the sequencer may produce.
    stop_after: AtomicU64,
}

impl Default for RpcBlockProductionControl {
    fn default() -> Self {
        Self {
            stop_after: AtomicU64::new(UNBOUNDED),
        }
    }
}

impl RpcBlockProductionControl {
    /// Stops production once the tip reaches `blocknum`.
    pub fn stop_after(&self, blocknum: u64) {
        // Relaxed: the value guards no other data.
        self.stop_after.store(blocknum, Ordering::Relaxed);
    }

    /// Removes the limit.
    pub fn clear(&self) {
        self.stop_after.store(UNBOUNDED, Ordering::Relaxed);
    }

    /// Returns the last block number production may reach, or `None` when
    /// unbounded.
    pub fn stop_after_blocknum(&self) -> Option<u64> {
        let stop_after = self.stop_after.load(Ordering::Relaxed);
        (stop_after != UNBOUNDED).then_some(stop_after)
    }

    fn allows_child_of(&self, parent_blocknum: u64) -> bool {
        parent_blocknum < self.stop_after.load(Ordering::Relaxed)
    }
}

impl BlockProductionControl for RpcBlockProductionControl {
    fn should_build_next_block(&self, parent: &ExecBlockRecord) -> bool {
        self.allows_child_of(parent.blocknum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unbounded() {
        let control = RpcBlockProductionControl::default();
        assert_eq!(control.stop_after_blocknum(), None);
        assert!(control.allows_child_of(0));
        assert!(control.allows_child_of(u64::MAX - 1));
    }

    #[test]
    fn stop_after_builds_through_target_block() {
        let control = RpcBlockProductionControl::default();
        control.stop_after(5);
        assert_eq!(control.stop_after_blocknum(), Some(5));
        assert!(control.allows_child_of(4));
        assert!(!control.allows_child_of(5));
        assert!(!control.allows_child_of(6));
    }

    #[test]
    fn stop_after_zero_stops_immediately() {
        let control = RpcBlockProductionControl::default();
        control.stop_after(0);
        assert_eq!(control.stop_after_blocknum(), Some(0));
        assert!(!control.allows_child_of(0));
    }

    #[test]
    fn clear_restores_unbounded() {
        let control = RpcBlockProductionControl::default();
        control.stop_after(5);
        control.clear();
        assert_eq!(control.stop_after_blocknum(), None);
        assert!(control.allows_child_of(5));
    }
}
