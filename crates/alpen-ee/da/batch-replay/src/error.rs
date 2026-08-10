//! Replay error types.

use alpen_reth_statediff::ReconstructError;

/// Error returned while applying replay batches to Ethereum state.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// Genesis state construction failed.
    #[error("genesis state construction failed: {source}")]
    GenesisState {
        #[source]
        source: ReconstructError,
    },

    /// The next batch did not match the expected update sequence number.
    #[error("unexpected update_seq_no (expected {expected}, got {actual})")]
    UnexpectedSeqNo { expected: u64, actual: u64 },

    /// A replay batch did not advance past the previously applied block.
    #[error(
        "block continuity violation at update_seq_no {update_seq_no}: expected block > {expected_after_block_num}, got {actual_block_num}"
    )]
    BlockContinuityViolation {
        update_seq_no: u64,
        expected_after_block_num: u64,
        actual_block_num: u64,
    },

    /// Replay refuses to apply `u64::MAX` because no following sequence number exists.
    #[error("terminal update_seq_no {update_seq_no}")]
    TerminalUpdateSeqNo { update_seq_no: u64 },

    /// State-diff application failed.
    #[error("state-diff apply failed: {0}")]
    ApplyDiff(#[from] ReconstructError),
}
