//! The metrics the DA providers emit, and their descriptions.
//!
//! Names surface with reth's `reth_` prefix on its `--metrics` endpoint; see
//! `alpen_sequencer::metrics` for the convention.

use metrics::{describe_counter, describe_histogram, Unit};

/// DA blobs built, by whether the aggregated state diff was `empty`.
pub const DA_BLOBS_BUILT: &str = "alpen_ee_da_blobs_built_total";
/// Blocks aggregated into each DA blob.
pub const DA_BLOB_BLOCKS: &str = "alpen_ee_da_blob_blocks";
/// Deployed bytecodes left out of a blob because an earlier batch published them.
pub const DA_BYTECODES_DEDUPED: &str = "alpen_ee_da_bytecodes_deduped_total";
/// Encoded bytes of each DA blob, as split into envelope chunks.
pub const DA_BLOB_BYTES: &str = "alpen_ee_da_blob_bytes";
/// Chunks each envelope carries.
pub const DA_ENVELOPE_CHUNKS: &str = "alpen_ee_da_envelope_chunks";
/// Chunked envelopes handed to the L1 writer.
pub const DA_ENVELOPES_SUBMITTED: &str = "alpen_ee_da_envelopes_submitted_total";
/// Envelope status checks by the lifecycle, by the envelope's L1 `status`.
pub const DA_STATUS_CHECKS: &str = "alpen_ee_da_status_checks_total";

/// Registers the description and unit of every metric above.
pub fn describe() {
    describe_counter!(
        DA_BLOBS_BUILT,
        "DA blobs built, by whether the state diff was empty"
    );
    describe_histogram!(DA_BLOB_BLOCKS, "Blocks aggregated into each DA blob");
    describe_counter!(
        DA_BYTECODES_DEDUPED,
        "Deployed bytecodes left out of a blob because an earlier batch published them"
    );
    describe_histogram!(
        DA_BLOB_BYTES,
        Unit::Bytes,
        "Encoded bytes of each DA blob, as split into envelope chunks"
    );
    describe_histogram!(DA_ENVELOPE_CHUNKS, "Chunks each envelope carries");
    describe_counter!(
        DA_ENVELOPES_SUBMITTED,
        "Chunked envelopes handed to the L1 writer"
    );
    describe_counter!(
        DA_STATUS_CHECKS,
        "Envelope status checks by the lifecycle, by L1 status"
    );
}
