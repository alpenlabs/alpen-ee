//! The metrics the sequencer workers emit, and their descriptions.
//!
//! Every instrument goes through the `metrics` facade; the binary installs
//! the recorder. With `alpen-client` that is reth's Prometheus recorder, which
//! prefixes every name with `reth_` and serves it on reth's `--metrics`
//! endpoint, so `alpen_ee_blocks_built_total` is scraped as
//! `reth_alpen_ee_blocks_built_total`.

use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

/// Blocks the block builder built, submitted and recorded.
pub const BLOCKS_BUILT: &str = "alpen_ee_blocks_built_total";
/// Block-build attempts that failed, by `reason`.
pub const BLOCK_BUILD_FAILURES: &str = "alpen_ee_block_build_failures_total";
/// Seconds one block took, by `stage`: `assemble` (payload built) and
/// `total` (payload built, submitted, stored and announced).
pub const BLOCK_BUILD_SECONDS: &str = "alpen_ee_block_build_seconds";
/// Seconds between a block's timestamp and its parent's.
pub const BLOCK_INTERVAL_SECONDS: &str = "alpen_ee_block_interval_seconds";
/// Encoded size of each block's execution payload.
pub const BLOCK_PAYLOAD_BYTES: &str = "alpen_ee_block_payload_bytes";
/// OL inbox messages (deposits) consumed by blocks.
pub const BLOCK_INBOX_MESSAGES: &str = "alpen_ee_block_inbox_messages_total";
/// Height of the chain at each `status`: `head` is the last block the sequencer
/// built. The engine reports `preconf`, `safe` and `finalized` under the same name.
pub const BLOCK_LATEST_HEIGHT: &str = "alpen_ee_block_latest_height";

/// Batches sealed, by the sealing `reason` (the policy that fired).
pub const BATCHES_SEALED: &str = "alpen_ee_batches_sealed_total";
/// Blocks per sealed batch.
pub const BATCH_BLOCKS: &str = "alpen_ee_batch_blocks";
/// Reorgs the batch builder reverted sealed batches for.
pub const BATCH_REORGS: &str = "alpen_ee_batch_reorgs_total";
/// Chunks sealed, by the sealing `reason`.
pub const CHUNKS_SEALED: &str = "alpen_ee_chunks_sealed_total";

/// Batch status transitions, by the `status` entered.
pub const BATCH_TRANSITIONS: &str = "alpen_ee_batch_transitions_total";
/// Seconds a batch took to reach `status` from the previous one.
pub const BATCH_STATUS_SECONDS: &str = "alpen_ee_batch_status_seconds";
/// Seconds from the lifecycle first seeing a sealed batch to its proof being ready.
pub const BATCH_SETTLEMENT_SECONDS: &str = "alpen_ee_batch_settlement_seconds";
/// Index of the latest batch at each lifecycle `status` (`sealed`, `da_pending`,
/// `da_complete`, `proof_pending`, `proof_ready`, `ol_latest`: included at the OL tip).
pub const BATCH_LATEST_IDX: &str = "alpen_ee_batch_latest_idx";
/// Lifecycle transitions that failed and will be retried, by the `status` attempted.
pub const BATCH_TRANSITION_FAILURES: &str = "alpen_ee_batch_transition_failures_total";

/// Snark account updates submitted to the OL, by `kind`: `first` for a batch's
/// first submission, `retry` when the OL had not yet accepted an earlier one.
pub const OL_UPDATES_SUBMITTED: &str = "alpen_ee_ol_updates_submitted_total";
/// Update submitter passes that failed: the OL state read, the update build or the submit.
pub const OL_UPDATE_FAILURES: &str = "alpen_ee_ol_update_failures_total";
/// Outputs carried by submitted updates, by `kind` (`transfer`, `message`).
pub const OL_UPDATE_OUTPUTS: &str = "alpen_ee_ol_update_outputs_total";

/// Registers the description and unit of every metric above.
///
/// Call once after the recorder is installed; descriptions recorded before
/// that are lost.
pub fn describe() {
    describe_counter!(
        BLOCKS_BUILT,
        "Blocks the block builder built, submitted and recorded"
    );
    describe_counter!(
        BLOCK_BUILD_FAILURES,
        "Block-build attempts that failed, by reason"
    );
    describe_histogram!(
        BLOCK_BUILD_SECONDS,
        Unit::Seconds,
        "Seconds one block took, by stage (assemble, total)"
    );
    describe_histogram!(
        BLOCK_INTERVAL_SECONDS,
        Unit::Seconds,
        "Seconds between a block's timestamp and its parent's"
    );
    describe_histogram!(
        BLOCK_PAYLOAD_BYTES,
        Unit::Bytes,
        "Encoded size of each block's execution payload"
    );
    describe_counter!(
        BLOCK_INBOX_MESSAGES,
        "OL inbox messages (deposits) consumed by blocks"
    );
    describe_gauge!(
        BLOCK_LATEST_HEIGHT,
        "Height of the chain at each status (head, preconf, safe, finalized)"
    );
    describe_counter!(
        BATCHES_SEALED,
        "Batches sealed, by the sealing policy that fired"
    );
    describe_histogram!(BATCH_BLOCKS, "Blocks per sealed batch");
    describe_counter!(
        BATCH_REORGS,
        "Reorgs the batch builder reverted sealed batches for"
    );
    describe_counter!(
        CHUNKS_SEALED,
        "Chunks sealed, by the sealing policy that fired"
    );
    describe_counter!(
        BATCH_TRANSITIONS,
        "Batch status transitions, by the status entered"
    );
    describe_histogram!(
        BATCH_STATUS_SECONDS,
        Unit::Seconds,
        "Seconds a batch took to reach each status from the previous one"
    );
    describe_histogram!(
        BATCH_SETTLEMENT_SECONDS,
        Unit::Seconds,
        "Seconds from a sealed batch being seen to its proof being ready"
    );
    describe_gauge!(
        BATCH_LATEST_IDX,
        "Index of the latest batch at each lifecycle status; the gap between two statuses is that stage's backlog"
    );
    describe_counter!(
        BATCH_TRANSITION_FAILURES,
        "Lifecycle transitions that failed and will be retried, by the status attempted"
    );
    describe_counter!(
        OL_UPDATES_SUBMITTED,
        "Snark account updates submitted to the OL, by kind (first, retry)"
    );
    describe_counter!(
        OL_UPDATE_FAILURES,
        "Update submissions the OL refused or that failed"
    );
    describe_counter!(
        OL_UPDATE_OUTPUTS,
        "Outputs carried by submitted updates, by kind"
    );
}
