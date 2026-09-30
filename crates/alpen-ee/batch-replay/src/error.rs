//! Batch replay error types.

use alpen_reth_statediff::ReconstructError;
use strata_identifiers::Buf32;
use strata_snark_acct_types::Seqno;

/// Error returned while replaying batches against Ethereum state.
#[derive(Debug, thiserror::Error)]
pub enum BatchReplayError {
    /// Genesis state construction failed.
    #[error("genesis state construction failed: {source}")]
    GenesisState {
        #[source]
        source: ReconstructError,
    },

    /// Replay requires at least one batch.
    #[error("no replay batches supplied")]
    NoBatches,

    /// The decoded snapshot state did not reproduce its saved state root.
    #[error("snapshot state root mismatch (expected {expected}, got {actual})")]
    SnapshotRootMismatch { expected: Buf32, actual: Buf32 },

    /// The next batch did not match the expected update sequence number.
    #[error(
        "unexpected update sequence number (expected {}, got {})",
        .expected.inner(),
        .actual.inner()
    )]
    UnexpectedSeqNo { expected: Seqno, actual: Seqno },

    /// A replay batch did not advance past the previously applied block.
    #[error(
        "block continuity violation at update sequence number {} (expected block > {expected_after_block_num}, got {actual_block_num})",
        .update_seq_no.inner()
    )]
    BlockContinuityViolation {
        update_seq_no: Seqno,
        expected_after_block_num: u64,
        actual_block_num: u64,
    },

    /// Replay refuses to apply `u64::MAX` because no following sequence number exists.
    #[error("terminal update sequence number {}", .update_seq_no.inner())]
    TerminalUpdateSeqNo { update_seq_no: Seqno },

    /// State-diff application failed.
    #[error("state-diff apply failed: {0}")]
    ApplyDiff(#[from] ReconstructError),
}
