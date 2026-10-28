//! Tracks and manages the OL chain state for Alpen execution environment.

mod epoch;

pub use epoch::{
    init_ol_tracker_state, EpochTrackingMode, OLTrackerService, OLTrackerServiceState,
    OLTrackerState, OLTrackerStatus,
};
