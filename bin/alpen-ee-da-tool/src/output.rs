//! Machine-readable command output.

use alpen_ee_da_l1_extraction::EeDaL1Ref;
use alpen_ee_l1_reconstruction::L1ReconstructionOutcome;
use bitcoin::Txid;
use serde::Serialize;
use strata_identifiers::{Buf32, L1BlockCommitment};

/// Identifies an applied EVM update and its L1 provenance.
#[derive(Serialize)]
struct AppliedUpdateRef {
    update_seq_no: u64,
    commit_txid: Txid,
    commit_block: L1BlockCommitment,
    completion_block: L1BlockCommitment,
}

impl AppliedUpdateRef {
    fn new(update_seq_no: u64, l1_ref: EeDaL1Ref, commit_block: L1BlockCommitment) -> Self {
        Self {
            update_seq_no,
            commit_txid: l1_ref.commit_txid(),
            commit_block,
            completion_block: l1_ref.completion_block(),
        }
    }
}

/// Reconstructed EVM state summary.
#[derive(Serialize)]
pub(crate) struct EvmStateReconstructionSummary {
    first_update: AppliedUpdateRef,
    last_update: AppliedUpdateRef,
    first_block_num: u64,
    last_block_num: u64,
    evm_state_root: Buf32,
}

/// Result of an EVM state reconstruction attempt.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum EvmStateReconstructionOutcome {
    /// No EE DA blobs were recovered for reconstruction.
    NoEeDaBlobsFound,

    /// Contains the summary of a completed EVM state reconstruction.
    Complete(Box<EvmStateReconstructionSummary>),
}

impl EvmStateReconstructionOutcome {
    /// Creates output for a completed reconstruction.
    pub(crate) fn complete(
        l1_reconstruction_outcome: &L1ReconstructionOutcome,
        first_commit_block: L1BlockCommitment,
        last_commit_block: L1BlockCommitment,
    ) -> Self {
        let batch_replay_outcome = l1_reconstruction_outcome.batch_replay_outcome();
        let applied_range = batch_replay_outcome.applied_range();
        Self::Complete(Box::new(EvmStateReconstructionSummary {
            first_update: AppliedUpdateRef::new(
                *applied_range.first_update_seq_no().inner(),
                l1_reconstruction_outcome.first_batch_l1_ref(),
                first_commit_block,
            ),
            last_update: AppliedUpdateRef::new(
                *applied_range.last_update_seq_no().inner(),
                l1_reconstruction_outcome.last_batch_l1_ref(),
                last_commit_block,
            ),
            first_block_num: applied_range.first_block_num(),
            last_block_num: applied_range.last_block_num(),
            evm_state_root: batch_replay_outcome.final_state_root(),
        }))
    }
}

/// Prints `value` as formatted JSON.
pub(crate) fn emit<T: Serialize>(value: &T) -> eyre::Result<()> {
    let rendered = serde_json::to_string_pretty(value)?;
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;
    use strata_identifiers::L1BlockId;

    use super::*;

    fn build_applied_update(
        update_seq_no: u64,
        seed: u8,
        commit_height: u32,
        completion_height: u32,
    ) -> AppliedUpdateRef {
        AppliedUpdateRef {
            update_seq_no,
            commit_txid: Txid::from_byte_array([seed; 32]),
            commit_block: L1BlockCommitment::new(commit_height, L1BlockId::default()),
            completion_block: L1BlockCommitment::new(completion_height, L1BlockId::default()),
        }
    }

    #[test]
    fn test_complete_outcome_serialization() {
        let output =
            EvmStateReconstructionOutcome::Complete(Box::new(EvmStateReconstructionSummary {
                first_update: build_applied_update(1, 0x11, 5, 6),
                last_update: build_applied_update(2, 0x22, 7, 8),
                first_block_num: 3,
                last_block_num: 4,
                evm_state_root: Buf32::new([0xab; 32]),
            }));

        let value = serde_json::to_value(output).expect("reconstruction outcome serializes");

        assert_eq!(value["status"], "complete");
        assert_eq!(value["evm_state_root"], "ab".repeat(32));
        assert_eq!(value["first_update"]["update_seq_no"], 1);
        assert_eq!(value["first_update"]["commit_txid"], "11".repeat(32));
        assert_eq!(value["first_update"]["commit_block"]["height"], 5);
        assert_eq!(value["first_update"]["completion_block"]["height"], 6);
        assert_eq!(value["last_update"]["update_seq_no"], 2);
        assert_eq!(value["last_update"]["commit_txid"], "22".repeat(32));
        assert_eq!(value["last_update"]["commit_block"]["height"], 7);
        assert_eq!(value["last_update"]["completion_block"]["height"], 8);
    }
}
