//! The metrics the DA providers emit, and their descriptions.
//!
//! Names surface with reth's `reth_` prefix on its `--metrics` endpoint; see
//! `alpen_sequencer::metrics` for the convention.

use metrics::{describe_counter, describe_histogram, Unit};

/// Deployed bytecodes left out of a blob because an earlier batch published them.
/// Counted per blob build; a post that is retried builds the blob again.
pub const DA_BYTECODES_DEDUPED: &str = "alpen_ee_da_bytecodes_deduped_total";
/// Encoded bytes of each DA blob, as split into envelope chunks.
pub const DA_BLOB_BYTES: &str = "alpen_ee_da_blob_bytes";
/// Chunks each envelope carries.
pub const DA_ENVELOPE_CHUNKS: &str = "alpen_ee_da_envelope_chunks";
/// Chunked envelopes handed to the L1 writer, by whether the blob's state diff
/// was `empty`. One per batch posted.
pub const DA_ENVELOPES_SUBMITTED: &str = "alpen_ee_da_envelopes_submitted_total";
/// Envelope status checks by the lifecycle, by the envelope's L1 `status`.
pub const DA_STATUS_CHECKS: &str = "alpen_ee_da_status_checks_total";

/// Registers the description and unit of every metric above.
pub fn describe() {
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
