//! Machine-readable command output.

use alpen_ee_acct_state::compute_ee_account_inner_root;
use alpen_ee_da_l1_extraction::EeDaL1Ref;
use alpen_ee_l1_reconstruction::L1ReconstructionOutcome;
use bitcoin::Txid;
use serde::Serialize;
use strata_identifiers::{Buf32, L1BlockCommitment};

use crate::account_state::AccountStateVerification;

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

/// Summary of EVM state reconstructed from EE DA payloads.
#[derive(Serialize)]
struct EvmStateReconstructionSummary {
    first_update: AppliedUpdateRef,
    last_update: AppliedUpdateRef,
    first_block_num: u64,
    last_block_num: u64,
    evm_state_root: Buf32,
}

/// Summary of EE account-state verification against OL-published roots.
#[derive(Serialize)]
struct EeAccountStateVerificationSummary {
    initial_next_inbox_msg_idx: u64,
    final_next_inbox_msg_idx: u64,
    expected_inner_state_root: Buf32,
    reconstructed_inner_state_root: Buf32,
}

/// Summary of successfully reconstructed and verified EE state.
#[derive(Serialize)]
pub(crate) struct EeDaVerificationSummary {
    evm_state_reconstruction: EvmStateReconstructionSummary,
    ee_account_state_verification: EeAccountStateVerificationSummary,
}

/// Result of reconstructing and verifying EE state from L1 DA.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum EeDaVerificationOutcome {
    /// No EE DA blobs were recovered for reconstruction.
    NoEeDaBlobsFound,

    /// Contains the summary of successful EVM reconstruction and EE account verification.
    Verified(Box<EeDaVerificationSummary>),
}

impl EeDaVerificationOutcome {
    /// Creates output for successfully reconstructed and verified EE state.
    pub(crate) fn verified(
        reconstruction_outcome: &L1ReconstructionOutcome,
        account_state_verification: &AccountStateVerification,
        first_commit_block: L1BlockCommitment,
        last_commit_block: L1BlockCommitment,
    ) -> Self {
        let batch_replay_outcome = reconstruction_outcome.batch_replay_outcome();
        let applied_range = batch_replay_outcome.applied_range();
        let verified_state = account_state_verification.verified_state();
        let reconstructed_inner_state_root = compute_ee_account_inner_root(verified_state.state());
        Self::Verified(Box::new(EeDaVerificationSummary {
            evm_state_reconstruction: EvmStateReconstructionSummary {
                first_update: AppliedUpdateRef::new(
                    *applied_range.first_update_seq_no().inner(),
                    reconstruction_outcome.first_batch_l1_ref(),
                    first_commit_block,
                ),
                last_update: AppliedUpdateRef::new(
                    *applied_range.last_update_seq_no().inner(),
                    reconstruction_outcome.last_batch_l1_ref(),
                    last_commit_block,
                ),
                first_block_num: applied_range.first_block_num(),
                last_block_num: applied_range.last_block_num(),
                evm_state_root: batch_replay_outcome.final_state_root(),
            },
            ee_account_state_verification: EeAccountStateVerificationSummary {
                initial_next_inbox_msg_idx: 0,
                final_next_inbox_msg_idx: verified_state.next_inbox_msg_idx(),
                expected_inner_state_root: Buf32::new(
                    account_state_verification.expected_inner_state_root().0,
                ),
                reconstructed_inner_state_root: Buf32::new(reconstructed_inner_state_root.0),
            },
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
    fn test_verified_outcome_serialization() {
        let output = EeDaVerificationOutcome::Verified(Box::new(EeDaVerificationSummary {
            evm_state_reconstruction: EvmStateReconstructionSummary {
                first_update: build_applied_update(1, 0x11, 5, 6),
                last_update: build_applied_update(2, 0x22, 7, 8),
                first_block_num: 3,
                last_block_num: 4,
                evm_state_root: Buf32::new([0xab; 32]),
            },
            ee_account_state_verification: EeAccountStateVerificationSummary {
                initial_next_inbox_msg_idx: 5,
                final_next_inbox_msg_idx: 8,
                expected_inner_state_root: Buf32::new([0xcd; 32]),
                reconstructed_inner_state_root: Buf32::new([0xcd; 32]),
            },
        }));

        let value = serde_json::to_value(output).expect("verification outcome serializes");

        assert_eq!(value["status"], "verified");
        assert_eq!(
            value["evm_state_reconstruction"]["evm_state_root"],
            "ab".repeat(32)
        );
        assert_eq!(
            value["evm_state_reconstruction"]["first_update"]["update_seq_no"],
            1
        );
        assert_eq!(
            value["evm_state_reconstruction"]["first_update"]["commit_txid"],
            "11".repeat(32)
        );
        assert_eq!(
            value["evm_state_reconstruction"]["first_update"]["commit_block"]["height"],
            5
        );
        assert_eq!(
            value["evm_state_reconstruction"]["first_update"]["completion_block"]["height"],
            6
        );
        assert_eq!(
            value["evm_state_reconstruction"]["last_update"]["update_seq_no"],
            2
        );
        assert_eq!(
            value["evm_state_reconstruction"]["last_update"]["commit_txid"],
            "22".repeat(32)
        );
        assert_eq!(
            value["evm_state_reconstruction"]["last_update"]["commit_block"]["height"],
            7
        );
        assert_eq!(
            value["evm_state_reconstruction"]["last_update"]["completion_block"]["height"],
            8
        );
        assert_eq!(
            value["ee_account_state_verification"]["expected_inner_state_root"],
            "cd".repeat(32)
        );
        assert_eq!(
            value["ee_account_state_verification"]["reconstructed_inner_state_root"],
            "cd".repeat(32)
        );
    }
}
