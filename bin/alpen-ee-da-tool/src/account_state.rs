//! Verifies replayed EVM roots against OL-published EE account updates.

use alpen_ee_acct_state::{
    apply_ee_account_update_manifest, EeAccountReconstructionError, EeAccountUpdateManifest,
};
use alpen_ee_batch_replay::{AppliedBatchRoot, BatchReplayOutcome};
use alpen_ee_genesis::build_genesis_ee_account_state;
use alpen_ee_params::AlpenParams;
use async_trait::async_trait;
use strata_acct_types::{AccountId, Hash, MessageEntry};
use strata_ee_acct_types::EeAccountState;
use strata_snark_acct_types::Seqno;
use thiserror::Error;

use crate::progress::EeAccountVerificationProgress;

/// Represents an OL-published EE account update and the inbox messages it consumes.
pub(crate) struct OLAccountUpdate {
    manifest: EeAccountUpdateManifest,
    inbox_messages: Vec<MessageEntry>,
}

impl OLAccountUpdate {
    pub(crate) fn new(
        manifest: EeAccountUpdateManifest,
        inbox_messages: Vec<MessageEntry>,
    ) -> Self {
        Self {
            manifest,
            inbox_messages,
        }
    }

    pub(crate) fn manifest(&self) -> &EeAccountUpdateManifest {
        &self.manifest
    }

    pub(crate) fn inbox_messages(&self) -> &[MessageEntry] {
        &self.inbox_messages
    }
}

/// Supplies OL-published EE account updates for local verification.
#[async_trait]
pub(crate) trait OLAccountUpdateSource: Send + Sync {
    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> eyre::Result<OLAccountUpdate>;
}

/// Verified EE account state and metadata describing this verification run.
pub(crate) struct VerifiedAccountState {
    state: EeAccountState,
    next_inbox_msg_idx: u64,
    expected_inner_state_root: Hash,
}

impl VerifiedAccountState {
    fn new(
        state: EeAccountState,
        next_inbox_msg_idx: u64,
        expected_inner_state_root: Hash,
    ) -> Self {
        Self {
            state,
            next_inbox_msg_idx,
            expected_inner_state_root,
        }
    }

    /// Returns the verified EE account state.
    pub(crate) fn state(&self) -> &EeAccountState {
        &self.state
    }

    /// Returns the inbox cursor after this verification run.
    pub(crate) fn next_inbox_msg_idx(&self) -> u64 {
        self.next_inbox_msg_idx
    }

    /// Returns the final EE account inner-state root published by OL.
    pub(crate) fn expected_inner_state_root(&self) -> Hash {
        self.expected_inner_state_root
    }
}

/// Failure to verify an OL-published EE account update against local reconstruction.
#[derive(Debug, Error)]
enum AccountStateVerificationError {
    /// The OL update does not correspond to the locally replayed batch.
    #[error(
        "OL update sequence number mismatch (expected {}, got {})",
        .expected.inner(),
        .actual.inner()
    )]
    UpdateSeqNoMismatch { expected: Seqno, actual: Seqno },

    /// The update's initial inbox cursor does not match reconstructed account progress.
    #[error(
        "OL inbox cursor mismatch at update sequence number {} (expected {expected}, got {actual})",
        .update_seq_no.inner()
    )]
    InboxCursorMismatch {
        update_seq_no: Seqno,
        expected: u64,
        actual: u64,
    },

    /// Applying the update failed account-state reconstruction checks.
    #[error(
        "failed to apply OL update sequence number {}: {source}",
        .update_seq_no.inner()
    )]
    ApplyUpdate {
        update_seq_no: Seqno,
        #[source]
        source: EeAccountReconstructionError,
    },
}

/// Reconstructs EE account state from genesis and verifies each OL-published update.
struct GenesisAccountVerifier {
    state: EeAccountState,
    next_inbox_msg_idx: u64,
    expected_inner_state_root: Option<Hash>,
}

impl GenesisAccountVerifier {
    fn new(params: &AlpenParams) -> Self {
        Self {
            state: build_genesis_ee_account_state(params),
            next_inbox_msg_idx: 0,
            expected_inner_state_root: None,
        }
    }

    /// Verifies and applies an OL account update against its corresponding replayed EVM batch.
    fn apply_update(
        &mut self,
        applied_batch_root: &AppliedBatchRoot,
        update: &OLAccountUpdate,
    ) -> Result<(), AccountStateVerificationError> {
        let manifest = update.manifest();
        if manifest.update_seq_no() != applied_batch_root.update_seq_no() {
            return Err(AccountStateVerificationError::UpdateSeqNoMismatch {
                expected: applied_batch_root.update_seq_no(),
                actual: manifest.update_seq_no(),
            });
        }
        if manifest.prev_next_msg_idx() != self.next_inbox_msg_idx {
            return Err(AccountStateVerificationError::InboxCursorMismatch {
                update_seq_no: manifest.update_seq_no(),
                expected: self.next_inbox_msg_idx,
                actual: manifest.prev_next_msg_idx(),
            });
        }

        let evm_state_root = Hash::new(applied_batch_root.post_state_root().0);
        apply_ee_account_update_manifest(
            &mut self.state,
            evm_state_root,
            manifest,
            update.inbox_messages(),
        )
        .map_err(|source| AccountStateVerificationError::ApplyUpdate {
            update_seq_no: manifest.update_seq_no(),
            source,
        })?;

        self.next_inbox_msg_idx = manifest.new_next_msg_idx();
        self.expected_inner_state_root = Some(manifest.expected_inner_state_root());
        Ok(())
    }

    fn into_verified_state(self) -> VerifiedAccountState {
        let expected_inner_state_root = self
            .expected_inner_state_root
            .expect("batch replay outcome contains at least one applied root");
        VerifiedAccountState::new(
            self.state,
            self.next_inbox_msg_idx,
            expected_inner_state_root,
        )
    }
}

pub(crate) async fn verify_account_state_from_genesis(
    params: &AlpenParams,
    batch_replay_outcome: &BatchReplayOutcome,
    source: &impl OLAccountUpdateSource,
) -> eyre::Result<VerifiedAccountState> {
    let account_id = params.strata_exec_account_id();
    let mut verifier = GenesisAccountVerifier::new(params);
    let progress = EeAccountVerificationProgress::new(batch_replay_outcome.applied_roots().len());

    for applied_batch_root in batch_replay_outcome.applied_roots() {
        let update_seq_no = applied_batch_root.update_seq_no();
        progress.fetching_update(update_seq_no);
        let update = source
            .fetch_account_update(account_id, update_seq_no)
            .await?;
        progress.verifying_update(update_seq_no);
        verifier.apply_update(applied_batch_root, &update)?;
        progress.update_verified(update_seq_no);
    }

    progress.finish();
    Ok(verifier.into_verified_state())
}

#[cfg(test)]
mod tests {
    use alpen_ee_acct_state::compute_ee_account_inner_root;
    use alpen_ee_batch_replay::{replay_from_genesis, EvmReplayBatch};
    use alpen_ee_da_types::EvmHeaderSummary;
    use alpen_reth_statediff::BatchStateDiff;
    use strata_ee_acct_types::UpdateExtraData;

    use super::*;

    fn replay_empty_batch() -> BatchReplayOutcome {
        replay_from_genesis(
            [],
            [EvmReplayBatch::new(
                Seqno::zero(),
                EvmHeaderSummary {
                    block_num: 1,
                    timestamp: 1,
                    base_fee: 1,
                    gas_used: 0,
                    gas_limit: 1,
                },
                BatchStateDiff::new(),
            )],
        )
        .expect("batch replays")
    }

    fn build_account_update(
        update_seq_no: Seqno,
        prev_next_msg_idx: u64,
        expected_root: Hash,
        last_exec_blkid: Hash,
    ) -> OLAccountUpdate {
        OLAccountUpdate::new(
            EeAccountUpdateManifest::try_new(
                update_seq_no,
                expected_root,
                prev_next_msg_idx,
                prev_next_msg_idx,
                UpdateExtraData::new(last_exec_blkid, Hash::zero(), 0, 0),
            )
            .expect("test manifest has valid inbox cursors"),
            Vec::new(),
        )
    }

    #[test]
    fn test_matching_manifest_updates_account_state() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let applied_batch_root = &batch_replay_outcome.applied_roots()[0];
        let last_exec_blkid = Hash::new([7; 32]);
        let expected_state = EeAccountState::new(
            last_exec_blkid,
            Hash::new(applied_batch_root.post_state_root().0),
            Vec::new(),
            Vec::new(),
        );
        let update = build_account_update(
            Seqno::zero(),
            0,
            compute_ee_account_inner_root(&expected_state),
            last_exec_blkid,
        );
        let mut verifier = GenesisAccountVerifier::new(&params);

        verifier
            .apply_update(applied_batch_root, &update)
            .expect("matching manifest verifies");

        let verified_state = verifier.into_verified_state();
        assert_eq!(verified_state.state(), &expected_state);
        assert_eq!(verified_state.next_inbox_msg_idx(), 0);
        assert_eq!(
            verified_state.expected_inner_state_root(),
            compute_ee_account_inner_root(&expected_state)
        );
    }

    #[test]
    fn test_inbox_cursor_mismatch_rejected() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let update = build_account_update(Seqno::zero(), 1, Hash::zero(), Hash::new([7; 32]));
        let mut verifier = GenesisAccountVerifier::new(&params);

        let error = verifier
            .apply_update(&batch_replay_outcome.applied_roots()[0], &update)
            .expect_err("inbox cursor mismatch must fail");

        assert!(matches!(
            error,
            AccountStateVerificationError::InboxCursorMismatch {
                update_seq_no,
                expected: 0,
                actual: 1,
            } if update_seq_no == Seqno::zero()
        ));
        assert_eq!(verifier.state, build_genesis_ee_account_state(&params));
        assert_eq!(verifier.next_inbox_msg_idx, 0);
    }

    #[test]
    fn test_update_sequence_number_mismatch_rejected() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let update = build_account_update(Seqno::new(1), 0, Hash::zero(), Hash::new([7; 32]));
        let mut verifier = GenesisAccountVerifier::new(&params);

        let error = verifier
            .apply_update(&batch_replay_outcome.applied_roots()[0], &update)
            .expect_err("update sequence number mismatch must fail");

        assert!(matches!(
            error,
            AccountStateVerificationError::UpdateSeqNoMismatch { expected, actual }
                if expected == Seqno::zero() && actual == Seqno::new(1)
        ));
        assert_eq!(verifier.state, build_genesis_ee_account_state(&params));
        assert_eq!(verifier.next_inbox_msg_idx, 0);
    }

    #[test]
    fn test_inner_root_mismatch_rejected() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let update = build_account_update(Seqno::zero(), 0, Hash::zero(), Hash::new([7; 32]));
        let mut verifier = GenesisAccountVerifier::new(&params);

        let error = verifier
            .apply_update(&batch_replay_outcome.applied_roots()[0], &update)
            .expect_err("inner-root mismatch must fail");

        assert!(matches!(
            error,
            AccountStateVerificationError::ApplyUpdate {
                update_seq_no,
                source: EeAccountReconstructionError::InnerStateRootMismatch { .. },
            } if update_seq_no == Seqno::zero()
        ));
        assert_eq!(verifier.state, build_genesis_ee_account_state(&params));
        assert_eq!(verifier.next_inbox_msg_idx, 0);
    }
}
