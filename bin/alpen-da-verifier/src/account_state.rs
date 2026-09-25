//! Verifies replayed EVM roots against OL-published EE account updates.

use std::error::Error;

use alpen_acct_state::{
    reconstruct_ee_account_update, EeAccountReconstructionError, EeAccountUpdateData,
};
use alpen_acct_types::EeAccountState;
use alpen_batch_replay::{AppliedBatchRoot, BatchReplayOutcome};
use alpen_genesis::build_genesis_ee_account_state;
use alpen_params::AlpenParams;
use async_trait::async_trait;
use strata_acct_types::{AccountId, Hash, MessageEntry};
use strata_snark_acct_types::Seqno;
use thiserror::Error;

/// Represents an OL-published EE account transition and its optional state attestation.
#[derive(Clone, Debug)]
pub(crate) struct OLAccountUpdate {
    update: EeAccountUpdateData,
    expected_inner_state_root: Option<Hash>,
    inbox_messages: Vec<MessageEntry>,
}

impl OLAccountUpdate {
    pub(crate) fn new(
        update: EeAccountUpdateData,
        expected_inner_state_root: Option<Hash>,
        inbox_messages: Vec<MessageEntry>,
    ) -> Self {
        Self {
            update,
            expected_inner_state_root,
            inbox_messages,
        }
    }

    pub(crate) fn update(&self) -> &EeAccountUpdateData {
        &self.update
    }

    pub(crate) fn expected_inner_state_root(&self) -> Option<Hash> {
        self.expected_inner_state_root
    }

    pub(crate) fn inbox_messages(&self) -> &[MessageEntry] {
        &self.inbox_messages
    }
}

/// Supplies OL-published EE account updates for local verification.
#[async_trait]
pub(crate) trait OLAccountUpdateSource: Send + Sync {
    /// Fetches the next update sequence number in finalized OL account state.
    async fn fetch_finalized_next_update_seq_no(
        &self,
        account_id: AccountId,
    ) -> Result<Seqno, OLAccountUpdateError>;

    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, OLAccountUpdateError>;
}

/// Failure reported by an [`OLAccountUpdateSource`].
///
/// The source classifies its own failures, so verification stays independent of
/// how updates are fetched.
#[derive(Debug, Error)]
pub(crate) enum OLAccountUpdateError {
    /// A later verification cycle may succeed without intervention.
    #[error(transparent)]
    Transient(Box<dyn Error + Send + Sync>),

    /// Retrying cannot succeed without changing configuration or data.
    #[error(transparent)]
    Permanent(Box<dyn Error + Send + Sync>),
}

impl OLAccountUpdateError {
    /// Wraps a source failure as transient or permanent.
    pub(crate) fn new(source: impl Error + Send + Sync + 'static, recoverable: bool) -> Self {
        let source = Box::new(source);
        if recoverable {
            Self::Transient(source)
        } else {
            Self::Permanent(source)
        }
    }

    /// Returns whether retrying may succeed without changing verifier configuration or data.
    pub(crate) fn is_recoverable(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Verified EE account state and metadata describing this verification run.
#[derive(Clone, Debug)]
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

    fn into_parts(self) -> (EeAccountState, u64) {
        (self.state, self.next_inbox_msg_idx)
    }
}

/// Failure to verify an OL-published EE account update against local reconstruction.
#[derive(Debug, Error)]
pub(crate) enum AccountStateVerificationError {
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

    /// The selected verification prefix does not end at an OL-published root.
    #[error(
        "OL update sequence number {} ends the verification prefix without an inner-state root",
        .update_seq_no.inner()
    )]
    MissingTerminalAttestation { update_seq_no: Seqno },

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

/// Failure to fetch or verify an OL-published EE account update.
#[derive(Debug, Error)]
pub(crate) enum VerifyAccountStateError {
    /// Fetching the finalized OL account state failed.
    #[error("failed to fetch finalized OL account state: {source}")]
    FetchFinalizedAccountState {
        #[source]
        source: OLAccountUpdateError,
    },

    /// Fetching the OL update corresponding to a replayed batch failed.
    #[error(
        "failed to fetch OL account update for sequence number {}: {source}",
        .update_seq_no.inner()
    )]
    FetchAccountUpdate {
        update_seq_no: Seqno,
        #[source]
        source: OLAccountUpdateError,
    },

    /// The fetched OL update does not verify against local reconstruction.
    #[error(transparent)]
    VerifyUpdate(#[from] AccountStateVerificationError),
}

impl VerifyAccountStateError {
    /// Returns whether retrying may succeed without changing verifier configuration or data.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchFinalizedAccountState { source }
            | Self::FetchAccountUpdate { source, .. } => source.is_recoverable(),
            Self::VerifyUpdate(_) => false,
        }
    }
}

/// Reconstructs EE account state and verifies each available OL attestation.
struct AccountStateVerifier {
    state: EeAccountState,
    next_inbox_msg_idx: u64,
    expected_inner_state_root: Option<Hash>,
}

impl AccountStateVerifier {
    fn from_genesis(params: &AlpenParams) -> Self {
        Self {
            state: build_genesis_ee_account_state(params),
            next_inbox_msg_idx: 0,
            expected_inner_state_root: None,
        }
    }

    fn from_verified_state(verified_state: VerifiedAccountState) -> Self {
        let (state, next_inbox_msg_idx) = verified_state.into_parts();
        Self {
            state,
            next_inbox_msg_idx,
            expected_inner_state_root: None,
        }
    }

    /// Verifies and applies an OL account update against its corresponding replayed EVM batch.
    fn apply_update(
        &mut self,
        applied_batch_root: &AppliedBatchRoot,
        update: &OLAccountUpdate,
    ) -> Result<(), AccountStateVerificationError> {
        let account_update = update.update();
        if account_update.update_seq_no() != applied_batch_root.update_seq_no() {
            return Err(AccountStateVerificationError::UpdateSeqNoMismatch {
                expected: applied_batch_root.update_seq_no(),
                actual: account_update.update_seq_no(),
            });
        }
        if account_update.prev_next_msg_idx() != self.next_inbox_msg_idx {
            return Err(AccountStateVerificationError::InboxCursorMismatch {
                update_seq_no: account_update.update_seq_no(),
                expected: self.next_inbox_msg_idx,
                actual: account_update.prev_next_msg_idx(),
            });
        }

        let evm_state_root = Hash::new(applied_batch_root.post_state_root().0);
        let (candidate_state, reconstructed_inner_state_root) = reconstruct_ee_account_update(
            &self.state,
            evm_state_root,
            account_update,
            update.inbox_messages(),
        )
        .map_err(|source| AccountStateVerificationError::ApplyUpdate {
            update_seq_no: account_update.update_seq_no(),
            source,
        })?;
        if let Some(expected_inner_state_root) = update.expected_inner_state_root() {
            if reconstructed_inner_state_root != expected_inner_state_root {
                return Err(AccountStateVerificationError::ApplyUpdate {
                    update_seq_no: account_update.update_seq_no(),
                    source: EeAccountReconstructionError::InnerStateRootMismatch {
                        update_seq_no: account_update.update_seq_no(),
                        expected: expected_inner_state_root,
                        actual: reconstructed_inner_state_root,
                    },
                });
            }
        }

        self.state = candidate_state;
        self.next_inbox_msg_idx = account_update.new_next_msg_idx();
        self.expected_inner_state_root = update.expected_inner_state_root();
        Ok(())
    }

    fn into_verified_state(
        self,
        terminal_update_seq_no: Seqno,
    ) -> Result<VerifiedAccountState, AccountStateVerificationError> {
        let expected_inner_state_root = self.expected_inner_state_root.ok_or(
            AccountStateVerificationError::MissingTerminalAttestation {
                update_seq_no: terminal_update_seq_no,
            },
        )?;
        Ok(VerifiedAccountState::new(
            self.state,
            self.next_inbox_msg_idx,
            expected_inner_state_root,
        ))
    }
}

pub(crate) fn verify_account_state(
    params: &AlpenParams,
    batch_replay_outcome: &BatchReplayOutcome,
    account_updates: &[OLAccountUpdate],
    initial_state: Option<VerifiedAccountState>,
) -> Result<VerifiedAccountState, VerifyAccountStateError> {
    assert_eq!(
        batch_replay_outcome.applied_roots().len(),
        account_updates.len(),
        "each replayed batch must have one prefetched OL account update"
    );
    let mut verifier = match initial_state {
        Some(verified_state) => AccountStateVerifier::from_verified_state(verified_state),
        None => AccountStateVerifier::from_genesis(params),
    };
    for (applied_batch_root, update) in batch_replay_outcome
        .applied_roots()
        .iter()
        .zip(account_updates)
    {
        verifier.apply_update(applied_batch_root, update)?;
    }

    let terminal_update_seq_no = account_updates
        .last()
        .expect("batch replay outcome contains at least one applied root")
        .update()
        .update_seq_no();
    verifier
        .into_verified_state(terminal_update_seq_no)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use alpen_acct_state::compute_ee_account_inner_root;
    use alpen_acct_types::{PendingFinclEntry, UpdateExtraData};
    use alpen_batch_replay::{replay_from_genesis, EvmReplayBatch};
    use alpen_da_types::EvmHeaderSummary;
    use alpen_reth_statediff::BatchStateDiff;

    use super::*;

    /// Replays empty batches for sequences `0..=last_update_seq_no`.
    ///
    /// Replay always starts at genesis, so reaching a later sequence means
    /// replaying the ones before it.
    fn replay_empty_batches_through(last_update_seq_no: Seqno) -> BatchReplayOutcome {
        let batches = (0..=*last_update_seq_no.inner()).map(|sequence| {
            EvmReplayBatch::new(
                Seqno::new(sequence),
                EvmHeaderSummary {
                    block_num: sequence + 1,
                    timestamp: sequence + 1,
                    base_fee: 1,
                    gas_used: 0,
                    gas_limit: 1,
                    da_rate: None,
                },
                BatchStateDiff::new(),
            )
        });

        replay_from_genesis([], batches).expect("batches replay")
    }

    /// Replays the account's first batch, where no inbox message has been
    /// consumed yet.
    fn replay_empty_batch() -> BatchReplayOutcome {
        replay_empty_batches_through(Seqno::zero())
    }

    fn build_account_update(
        update_seq_no: Seqno,
        prev_next_msg_idx: u64,
        expected_root: Hash,
        last_exec_blkid: Hash,
    ) -> OLAccountUpdate {
        build_account_update_processing_fincls(
            update_seq_no,
            prev_next_msg_idx,
            expected_root,
            last_exec_blkid,
            0,
        )
    }

    fn build_account_update_processing_fincls(
        update_seq_no: Seqno,
        prev_next_msg_idx: u64,
        expected_root: Hash,
        last_exec_blkid: Hash,
        processed_fincls: u32,
    ) -> OLAccountUpdate {
        OLAccountUpdate::new(
            EeAccountUpdateData::try_new(
                update_seq_no,
                prev_next_msg_idx,
                prev_next_msg_idx,
                UpdateExtraData::new(last_exec_blkid, Hash::zero(), 0, processed_fincls),
            )
            .expect("test manifest has valid inbox cursors"),
            Some(expected_root),
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
        let mut verifier = AccountStateVerifier::from_genesis(&params);

        verifier
            .apply_update(applied_batch_root, &update)
            .expect("matching manifest verifies");

        let verified_state = verifier
            .into_verified_state(Seqno::zero())
            .expect("the terminal update is attested");
        assert_eq!(verified_state.state(), &expected_state);
        assert_eq!(verified_state.next_inbox_msg_idx(), 0);
        assert_eq!(
            verified_state.expected_inner_state_root(),
            compute_ee_account_inner_root(&expected_state)
        );
    }

    #[test]
    fn test_update_not_continuing_verified_cursor_rejected() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let update = build_account_update(Seqno::zero(), 1, Hash::zero(), Hash::new([7; 32]));
        let mut verifier = AccountStateVerifier::from_genesis(&params);

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
    fn test_update_not_matching_replayed_batch_rejected() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batch();
        let update = build_account_update(Seqno::new(1), 0, Hash::zero(), Hash::new([7; 32]));
        let mut verifier = AccountStateVerifier::from_genesis(&params);

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
        let mut verifier = AccountStateVerifier::from_genesis(&params);

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

    #[test]
    fn test_unattested_update_invalidates_previous_attestation() {
        let params = AlpenParams::default();
        let batch_replay_outcome = replay_empty_batches_through(Seqno::new(1));
        let first_applied_root = &batch_replay_outcome.applied_roots()[0];
        let first_last_exec_blkid = Hash::new([7; 32]);
        let first_expected_state = EeAccountState::new(
            first_last_exec_blkid,
            Hash::new(first_applied_root.post_state_root().0),
            Vec::new(),
            Vec::new(),
        );
        let attested_update = build_account_update(
            Seqno::zero(),
            0,
            compute_ee_account_inner_root(&first_expected_state),
            first_last_exec_blkid,
        );
        let unattested_update = OLAccountUpdate::new(
            EeAccountUpdateData::try_new(
                Seqno::new(1),
                0,
                0,
                UpdateExtraData::new(Hash::new([8; 32]), Hash::zero(), 0, 0),
            )
            .expect("test update has valid inbox cursors"),
            None,
            Vec::new(),
        );
        let mut verifier = AccountStateVerifier::from_genesis(&params);

        verifier
            .apply_update(first_applied_root, &attested_update)
            .expect("attested transition verifies");
        verifier
            .apply_update(&batch_replay_outcome.applied_roots()[1], &unattested_update)
            .expect("unattested transition reconstructs");

        let error = verifier
            .into_verified_state(Seqno::new(1))
            .expect_err("unattested terminal update must not form verified state");

        assert!(matches!(
            error,
            AccountStateVerificationError::MissingTerminalAttestation { update_seq_no }
                if update_seq_no == Seqno::new(1)
        ));
    }

    #[test]
    fn test_resumed_verifier_continues_from_verified_state() {
        // Verification stopped after sequence 4, having consumed 42 inbox
        // messages, so the next update is sequence 5.
        const VERIFIED_THROUGH_SEQ_NO: u64 = 4;
        const RESUMED_INBOX_MSG_IDX: u64 = 42;

        let params = AlpenParams::default();
        let resumed_seq_no = Seqno::new(VERIFIED_THROUGH_SEQ_NO + 1);
        let batch_replay_outcome = replay_empty_batches_through(resumed_seq_no);
        let applied_batch_root = batch_replay_outcome
            .applied_roots()
            .last()
            .expect("replay applies the resumed sequence");
        // Previously verified state: an inbox cursor well past genesis, and a
        // pending fincl this update consumes. Genesis holds neither, so an
        // update built for this state cannot be applied from genesis.
        let resumed_state = EeAccountState::new(
            Hash::new([1; 32]),
            Hash::new([2; 32]),
            Vec::new(),
            vec![PendingFinclEntry::new(9, Hash::new([9; 32]))],
        );
        let last_exec_blkid = Hash::new([7; 32]);
        let expected_state = EeAccountState::new(
            last_exec_blkid,
            Hash::new(applied_batch_root.post_state_root().0),
            Vec::new(),
            Vec::new(),
        );
        let update = build_account_update_processing_fincls(
            resumed_seq_no,
            RESUMED_INBOX_MSG_IDX,
            compute_ee_account_inner_root(&expected_state),
            last_exec_blkid,
            1,
        );
        let mut verifier = AccountStateVerifier::from_verified_state(VerifiedAccountState::new(
            resumed_state.clone(),
            RESUMED_INBOX_MSG_IDX,
            compute_ee_account_inner_root(&resumed_state),
        ));

        verifier
            .apply_update(applied_batch_root, &update)
            .expect("update continuing from the resumed cursor verifies");

        let next_verified_state = verifier
            .into_verified_state(resumed_seq_no)
            .expect("the terminal update is attested");
        assert_eq!(next_verified_state.state(), &expected_state);
        assert_eq!(
            next_verified_state.next_inbox_msg_idx(),
            RESUMED_INBOX_MSG_IDX
        );

        // Restoring the cursor alone is not enough: the same update consumes a
        // pending fincl that only the restored account state carries.
        let genesis_state = build_genesis_ee_account_state(&params);
        let mut cursor_only_verifier =
            AccountStateVerifier::from_verified_state(VerifiedAccountState::new(
                genesis_state.clone(),
                RESUMED_INBOX_MSG_IDX,
                compute_ee_account_inner_root(&genesis_state),
            ));

        let error = cursor_only_verifier
            .apply_update(applied_batch_root, &update)
            .expect_err("an update consuming a pending fincl must not apply without it");

        assert!(matches!(
            error,
            AccountStateVerificationError::ApplyUpdate {
                source: EeAccountReconstructionError::PendingFinclUnderflow {
                    requested: 1,
                    available: 0,
                    ..
                },
                ..
            }
        ));
    }
}
