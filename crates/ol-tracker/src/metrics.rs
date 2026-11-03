//! The metrics the OL tracker emits, and their descriptions.
//!
//! Names surface with reth's `reth_` prefix on its `--metrics` endpoint; see
//! `alpen_sequencer::metrics` for the convention.

use metrics::{describe_counter, describe_gauge};

/// OL epochs as the tracker last saw them, by `status`: `local` (applied here),
/// `confirmed`, `finalized` and `latest` (as the OL node reports them).
pub const OL_EPOCH: &str = "alpen_ee_ol_epoch";
/// OL epochs applied to the EE account state.
pub const OL_EPOCHS_APPLIED: &str = "alpen_ee_ol_epochs_applied_total";
/// Tracker poll outcomes, by `action` (`extend`, `refresh_finalized`, `reorg`, `noop`).
pub const OL_TRACKER_ACTIONS: &str = "alpen_ee_ol_tracker_actions_total";
/// Tracker polls that failed, by whether the error was `fatal`.
pub const OL_TRACKER_FAILURES: &str = "alpen_ee_ol_tracker_failures_total";

/// Registers the description of every metric above.
pub fn describe() {
    describe_gauge!(
        OL_EPOCH,
        "OL epochs as the tracker last saw them, by status (local, confirmed, finalized, latest)"
    );
    describe_counter!(
        OL_EPOCHS_APPLIED,
        "OL epochs applied to the EE account state"
    );
    describe_counter!(OL_TRACKER_ACTIONS, "Tracker poll outcomes, by action");
    describe_counter!(
        OL_TRACKER_FAILURES,
        "Tracker polls that failed, by whether the error was fatal"
    );
}
