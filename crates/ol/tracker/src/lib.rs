//! Tracks and manages the OL chain state for Alpen execution environment.
//!
//! The epoch tracker follows the OL chain epoch by epoch on every node. The inbox
//! tracker, which only a sequencer runs, follows finalized OL blocks and keeps their
//! inbox messages for block assembly.

mod epoch;
mod inbox;
mod status;

pub use epoch::{
    init_ol_tracker_state, start_ol_tracker_service, EpochTrackingMode, OLTrackerHandle,
    OLTrackerService, OLTrackerServiceState, OLTrackerState, OLTrackerStatus,
};
pub use inbox::{
    build_ol_chain_tracker, init_ol_chain_tracker_state, InboxMessages, OLChainTrackerHandle,
    OLChainTrackerState,
};
pub use status::OLFinalizedStatus;
