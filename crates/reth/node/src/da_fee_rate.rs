//! Read-only access to the DA fee rate used by payload construction.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Reads the latest controller-published DA fee rate.
///
/// The handle deliberately exposes no update operation. Reading returns an
/// owned snapshot so a later controller update cannot affect an in-progress
/// payload build.
#[derive(Clone, Debug)]
pub struct DaFeeRateHandle {
    current_rate: Arc<AtomicU64>,
}

impl DaFeeRateHandle {
    /// Returns the current rate in wei per DA byte.
    pub fn current_rate(&self) -> u64 {
        // The rate has no associated data whose visibility must be ordered
        // with this load, so relaxed atomic ordering is sufficient.
        self.current_rate.load(Ordering::Relaxed)
    }

    /// Creates a read-only handle whose rate never changes.
    pub fn fixed(rate_wei_per_byte: u64) -> Self {
        Self {
            current_rate: Arc::new(AtomicU64::new(rate_wei_per_byte)),
        }
    }
}

/// Publishes rates that become visible through a [`DaFeeRateHandle`].
///
/// The controller owns this capability and does not share it with payload
/// construction.
#[derive(Debug)]
pub struct DaFeeRateUpdater {
    current_rate: Arc<AtomicU64>,
}

impl DaFeeRateUpdater {
    /// Replaces the current rate and returns the previously published value.
    pub fn publish(&self, rate_wei_per_byte: u64) -> u64 {
        self.current_rate.swap(rate_wei_per_byte, Ordering::Relaxed)
    }
}

/// Creates the controller's update capability and its read-only consumer handle.
pub fn da_fee_rate_channel(initial_rate_wei_per_byte: u64) -> (DaFeeRateUpdater, DaFeeRateHandle) {
    let current_rate = Arc::new(AtomicU64::new(initial_rate_wei_per_byte));
    (
        DaFeeRateUpdater {
            current_rate: current_rate.clone(),
        },
        DaFeeRateHandle { current_rate },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_exposes_its_initial_rate() {
        let (_, handle) = da_fee_rate_channel(17);

        assert_eq!(handle.current_rate(), 17);
    }

    #[test]
    fn published_rate_is_visible_to_handle_clones() {
        let (updater, handle) = da_fee_rate_channel(17);
        let cloned_handle = handle.clone();

        assert_eq!(updater.publish(29), 17);
        assert_eq!(handle.current_rate(), 29);
        assert_eq!(cloned_handle.current_rate(), 29);
    }

    #[test]
    fn previously_read_snapshot_does_not_change() {
        let (updater, handle) = da_fee_rate_channel(17);
        let snapshot = handle.current_rate();

        updater.publish(29);

        assert_eq!(snapshot, 17);
        assert_eq!(handle.current_rate(), 29);
    }

    #[test]
    fn fixed_handle_keeps_its_rate() {
        let handle = DaFeeRateHandle::fixed(41);

        assert_eq!(handle.current_rate(), 41);
    }
}
