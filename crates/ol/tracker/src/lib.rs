//! Tracks and manages the OL chain state for Alpen execution environment.

mod epoch;

pub use epoch::{
    init_ol_tracker_state, start_ol_tracker_service, EpochTrackingMode, OLTrackerHandle,
    OLTrackerService, OLTrackerServiceState, OLTrackerState, OLTrackerStatus,
};
