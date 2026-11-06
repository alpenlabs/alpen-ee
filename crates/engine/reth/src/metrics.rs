//! The metrics the reth-backed engine emits, and their descriptions.
//!
//! Names surface with reth's `reth_` prefix on its `--metrics` endpoint; see
//! `alpen_sequencer::metrics` for the convention.

use alpen_common::metrics::BLOCK_LATEST_HEIGHT;
use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

/// Seconds a payload build took, by `stage`: `accept` (job accepted),
/// `resolve` (payload resolved after acceptance), `persist` (block witness
/// stored) and `total` (request through witness persistence).
pub const PAYLOAD_BUILD_SECONDS: &str = "alpen_ee_payload_build_seconds";
/// Payload builds retried because reth did not yet have the parent.
pub const PAYLOAD_BUILD_RETRIES: &str = "alpen_ee_payload_build_retries_total";
/// Payload builds that failed, by `reason`: `missing` (parent not in reth after
/// the retries), `rejected`, `communication`, `resolve_failed`, `no_payload`
/// (resolved to nothing), `witness_missing` (built without an inline witness)
/// and `persist` (witness store write failed).
pub const PAYLOAD_BUILD_FAILURES: &str = "alpen_ee_payload_build_failures_total";
/// Fork-choice updates sent to reth, by `kind`: `preconf` (new head),
/// `consensus` (OL safe and finalized heads) and `catchup` (resent heads).
pub const FORKCHOICE_UPDATES: &str = "alpen_ee_forkchoice_updates_total";
/// Fork-choice updates reth rejected or that failed.
pub const FORKCHOICE_FAILURES: &str = "alpen_ee_forkchoice_failures_total";

/// Registers the description and unit of every metric above.
pub fn describe() {
    describe_histogram!(
        PAYLOAD_BUILD_SECONDS,
        Unit::Seconds,
        "Seconds a payload build took, by stage (accept, resolve, persist, total)"
    );
    describe_counter!(
        PAYLOAD_BUILD_RETRIES,
        "Payload builds retried because reth did not yet have the parent"
    );
    describe_counter!(
        PAYLOAD_BUILD_FAILURES,
        "Payload builds that failed, by reason"
    );
    describe_counter!(
        FORKCHOICE_UPDATES,
        "Fork-choice updates sent to reth, by kind (preconf, consensus, catchup)"
    );
    describe_counter!(
        FORKCHOICE_FAILURES,
        "Fork-choice updates reth rejected or that failed"
    );
    describe_gauge!(
        BLOCK_LATEST_HEIGHT,
        "Height of the chain at each status (head, preconf, safe, finalized)"
    );
}
