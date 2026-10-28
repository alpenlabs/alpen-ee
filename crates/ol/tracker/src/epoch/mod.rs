//! Follows the OL chain epoch by epoch and keeps the EE account state for each one.

mod error;
mod handle;
mod reorg;
mod service;
mod state;
mod task;
#[cfg(test)]
pub(crate) mod test_utils;

pub use handle::{start_ol_tracker_service, OLTrackerHandle};
pub use service::{EpochTrackingMode, OLTrackerService, OLTrackerServiceState, OLTrackerStatus};
pub use state::{init_ol_tracker_state, OLTrackerState};
