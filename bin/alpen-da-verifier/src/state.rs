//! Mutable state and transitions for the EE DA verifier service.

use std::{num::NonZeroU32, sync::Arc};

use alpen_acct_state::compute_ee_account_inner_root;
use alpen_batch_replay::BatchReplaySnapshot;
use alpen_da_l1_extraction::{DaExtractor, DaL1Ref, RecoveredDaBlob};
use alpen_database::RecoveredDaDbError;
use alpen_l1_reconstruction::{L1ReconstructionError, L1ReconstructionOutcome};
use alpen_params::AlpenParams;
use strata_identifiers::{L1BlockCommitment, L1Height};
use strata_snark_acct_types::Seqno;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    account_state::{
        verify_account_state, OLAccountUpdate, VerifiedAccountState, VerifyAccountStateError,
    },
    bitcoin::{CheckL1BlockError, FetchBitcoinTipError, FetchCommitBlockError},
    context::DaVerifierContext,
    da_extraction::{DaRecoveryDriver, DaRecoveryError},
    evm_state::reconstruct_evm_state,
    snapshot::{ReconstructionSnapshot, SnapshotDeleteError, SnapshotLoadError, SnapshotSaveError},
};

/// Failure to complete one EE DA verification cycle.
#[derive(Debug, Error)]
pub(crate) enum DaVerifierError {
    /// Loading the persisted reconstruction snapshot failed.
    #[error(transparent)]
    LoadSnapshot(#[from] SnapshotLoadError),

    /// Deleting a stale reconstruction snapshot failed.
    #[error(transparent)]
    DeleteSnapshot(#[from] SnapshotDeleteError),

    /// Checking whether a snapshot's L1 anchor remains canonical failed.
    #[error(transparent)]
    CheckL1Block(#[from] CheckL1BlockError),

    /// Fetching the current Bitcoin tip failed.
    #[error(transparent)]
    FetchBitcoinTip(#[from] FetchBitcoinTipError),

    /// Looking up the block containing the last applied DA commit failed.
    #[error(transparent)]
    FetchCommitBlock(#[from] FetchCommitBlockError),

    /// Recovering or persisting EE DA failed.
    #[error(transparent)]
    DaRecovery(#[from] DaRecoveryError),

    /// Reading recovered EE DA from storage failed.
    #[error(transparent)]
    RecoveredDaDb(#[from] RecoveredDaDbError),

    /// Reconstructing EVM state failed.
    #[error(transparent)]
    Reconstruct(#[from] L1ReconstructionError),

    /// Reconstructing or verifying the EE account state failed.
    #[error(transparent)]
    VerifyAccountState(#[from] VerifyAccountStateError),

    /// Finalized OL state has not reached the verifier's tracked anchor.
    #[error(
        "finalized OL account next sequence {} is behind verifier's tracked next sequence {}",
        .finalized_next_update_seq_no.inner(),
        .anchor_next_update_seq_no.inner()
    )]
    LaggingFinalizedOLFrontier {
        anchor_next_update_seq_no: Seqno,
        finalized_next_update_seq_no: Seqno,
    },

    /// Persisting the verified state snapshot failed.
    #[error(transparent)]
    SaveSnapshot(#[from] SnapshotSaveError),
}

impl DaVerifierError {
    /// Returns whether a later verification cycle may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::LoadSnapshot(error) => error.is_recoverable(),
            Self::DeleteSnapshot(error) => error.is_recoverable(),
            Self::CheckL1Block(error) => error.is_recoverable(),
            Self::FetchBitcoinTip(error) => error.is_recoverable(),
            Self::FetchCommitBlock(error) => error.is_recoverable(),
            Self::DaRecovery(error) => error.is_recoverable(),
            Self::RecoveredDaDb(error) => error.is_recoverable(),
            Self::VerifyAccountState(error) => error.is_recoverable(),
            Self::LaggingFinalizedOLFrontier { .. } => true,
            Self::SaveSnapshot(error) => error.is_recoverable(),
            Self::Reconstruct(_) => false,
        }
    }

    /// Returns whether the failure originated from recovered-DA storage.
    fn is_database_failure(&self) -> bool {
        match self {
            Self::DaRecovery(error) => error.is_database_failure(),
            Self::RecoveredDaDb(_) => true,
            Self::FetchBitcoinTip(_)
            | Self::FetchCommitBlock(_)
            | Self::CheckL1Block(_)
            | Self::SaveSnapshot(_)
            | Self::LoadSnapshot(_)
            | Self::DeleteSnapshot(_)
            | Self::Reconstruct(_)
            | Self::VerifyAccountState(_)
            | Self::LaggingFinalizedOLFrontier { .. } => false,
        }
    }
}

/// Describes the DA recovery action implied by the current L1 frontier and recovery cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaRecoveryAction {
    WaitForReorgSafeTip,
    CaughtUp { reorg_safe_tip: L1Height },
    RecoverThrough { reorg_safe_tip: L1Height },
}

impl DaRecoveryAction {
    fn reorg_safe_tip(self) -> Option<L1Height> {
        match self {
            Self::WaitForReorgSafeTip => None,
            Self::CaughtUp { reorg_safe_tip } | Self::RecoverThrough { reorg_safe_tip } => {
                Some(reorg_safe_tip)
            }
        }
    }
}

/// Describes whether the current verifier tick recovers new DA or verifies persisted DA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaRecoveryResolution {
    RecoverThrough { reorg_safe_tip: L1Height },
    VerifyRecoveredDa,
}

/// Policy and mutable progress for recovering and persisting EE DA.
pub(crate) struct DaRecoveryState {
    l1_reorg_safe_depth: u32,
    max_l1_scan_window_size: NonZeroU32,
    genesis_l1_height: L1Height,
    driver: DaRecoveryDriver,

    /// Highest L1 height fully processed by recovery, or [`None`] before the first block.
    recovered_l1_frontier: Option<L1Height>,

    /// Published for status only; recomputed from the Bitcoin tip each cycle.
    reorg_safe_tip: Option<L1Height>,
}

impl DaRecoveryState {
    /// Creates recovery state anchored at the configured genesis L1 height.
    pub(crate) fn new(
        l1_reorg_safe_depth: u32,
        max_l1_scan_window_size: NonZeroU32,
        extractor: DaExtractor,
        genesis_l1_height: L1Height,
    ) -> Self {
        Self {
            l1_reorg_safe_depth,
            max_l1_scan_window_size,
            genesis_l1_height,
            driver: DaRecoveryDriver::new(extractor, genesis_l1_height),
            recovered_l1_frontier: None,
            reorg_safe_tip: None,
        }
    }

    /// Freezes the reorg-safe L1 target for the current tick.
    async fn recovery_action(
        &mut self,
        context: &impl DaVerifierContext,
    ) -> Result<DaRecoveryAction, DaVerifierError> {
        let previous_next_l1_height = self.driver.next_l1_height();
        self.driver.flush_pending(context).await?;
        self.update_recovered_l1_frontier(previous_next_l1_height);

        let tip_height = context.fetch_bitcoin_tip_height().await?;
        let action = decide_da_recovery_action(
            tip_height,
            self.l1_reorg_safe_depth,
            self.driver.next_l1_height(),
        );
        self.reorg_safe_tip = action.reorg_safe_tip();
        Ok(action)
    }

    async fn scan_next_l1_window_and_persist_recovered_da(
        &mut self,
        context: &impl DaVerifierContext,
        reorg_safe_tip: L1Height,
    ) -> Result<CompletedDaRecoveryRange, DaVerifierError> {
        let start_l1_height = self.driver.next_l1_height();
        let recovered_l1_frontier = l1_scan_window_end(
            start_l1_height,
            self.max_l1_scan_window_size,
            reorg_safe_tip,
        );
        let recovery_result = self
            .driver
            .recover_through(context, recovered_l1_frontier)
            .await;
        self.update_recovered_l1_frontier(start_l1_height);
        recovery_result?;

        Ok(CompletedDaRecoveryRange {
            start_l1_height,
            recovered_l1_frontier,
        })
    }

    fn recovered_l1_frontier(&self) -> Option<L1Height> {
        self.recovered_l1_frontier
    }

    fn update_recovered_l1_frontier(&mut self, previous_next_l1_height: L1Height) {
        let next_l1_height = self.driver.next_l1_height();
        if next_l1_height > previous_next_l1_height {
            self.recovered_l1_frontier = next_l1_height.checked_sub(1);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletedDaRecoveryRange {
    start_l1_height: L1Height,
    recovered_l1_frontier: L1Height,
}

/// EVM and account-state anchors tracked after successful OL verification.
struct VerifiedStateAnchor {
    replay_snapshot: BatchReplaySnapshot,
    account_state: VerifiedAccountState,
    pending_snapshot_l1_ref: Option<DaL1Ref>,
    completion_block: L1BlockCommitment,
}

impl VerifiedStateAnchor {
    fn from_snapshot(snapshot: ReconstructionSnapshot) -> Self {
        let completion_block = snapshot.completion_block();
        let (replay_snapshot, account_state) = snapshot.into_state_parts();
        Self {
            replay_snapshot,
            account_state,
            pending_snapshot_l1_ref: None,
            completion_block,
        }
    }

    fn next_update_seq_no(&self) -> u64 {
        *self.replay_snapshot.next_update_seq_no().inner()
    }
}

/// Policy and mutable progress for reconstructing and verifying EE state.
pub(crate) struct DaVerificationState {
    params: AlpenParams,

    /// Replay and account state at the last OL-verified update, if any.
    verified_state_anchor: Option<VerifiedStateAnchor>,
}

impl DaVerificationState {
    /// Creates verification state that begins replay from genesis.
    pub(crate) fn new(params: AlpenParams) -> Self {
        Self {
            params,
            verified_state_anchor: None,
        }
    }

    fn restore_snapshot(&mut self, snapshot: ReconstructionSnapshot) {
        self.verified_state_anchor = Some(VerifiedStateAnchor::from_snapshot(snapshot));
    }

    /// Returns the next EE update sequence number verification expects.
    fn next_update_seq_no(&self) -> u64 {
        self.verified_state_anchor
            .as_ref()
            .map_or(0, VerifiedStateAnchor::next_update_seq_no)
    }

    /// Returns whether verification has reached the finalized OL account frontier.
    fn verification_reached_finalized_ol(&self, finalized_next_update_seq_no: Seqno) -> bool {
        self.next_update_seq_no() >= *finalized_next_update_seq_no.inner()
    }

    /// Reconstructs and verifies the contiguous recovered prefix eligible at an L1 frontier.
    async fn reconstruct_and_verify_state_from_recovered_da(
        &mut self,
        context: &impl DaVerifierContext,
        recovered_l1_frontier: L1Height,
        first_unfinalized_update_seq_no_cache: &mut Option<Seqno>,
    ) -> Result<(), DaVerifierError> {
        let recovered_blobs = self
            .load_contiguous_da_prefix(context, recovered_l1_frontier)
            .await?;
        if recovered_blobs.is_empty() {
            debug!(
                recovered_l1_frontier,
                "no contiguous EE DA sequence is available for reconstruction"
            );
            return Ok(());
        }

        let (recovered_blobs, account_updates) = self
            .fetch_finalized_ol_prefix(
                context,
                recovered_blobs,
                first_unfinalized_update_seq_no_cache,
            )
            .await?;
        if recovered_blobs.is_empty() {
            return Ok(());
        }
        let completion_block = compute_highest_completion_block(
            self.verified_state_anchor
                .as_ref()
                .map(|anchor| anchor.completion_block),
            recovered_blobs
                .iter()
                .map(|recovered| recovered.l1_ref().completion_block()),
        )
        .expect("an OL-attested DA prefix contains at least one recovered blob");

        let replay_snapshot = self
            .verified_state_anchor
            .as_ref()
            .map(|anchor| anchor.replay_snapshot.clone());
        let initial_account_state = self
            .verified_state_anchor
            .as_ref()
            .map(|anchor| anchor.account_state.clone());
        let Some(l1_reconstruction_outcome) =
            reconstruct_evm_state(&self.params, replay_snapshot, recovered_blobs)?
        else {
            return Ok(());
        };
        let batch_replay_outcome = l1_reconstruction_outcome.batch_replay_outcome();
        let verified_account_state = verify_account_state(
            &self.params,
            batch_replay_outcome,
            &account_updates,
            initial_account_state,
        )?;

        self.advance_verified_state_anchor(
            l1_reconstruction_outcome,
            verified_account_state,
            completion_block,
        )?;
        Ok(())
    }

    async fn save_verified_state_snapshot(
        &mut self,
        context: &impl DaVerifierContext,
    ) -> Result<(), DaVerifierError> {
        let Some(anchor) = self.verified_state_anchor.as_ref() else {
            return Ok(());
        };
        let Some(pending_snapshot_l1_ref) = anchor.pending_snapshot_l1_ref else {
            return Ok(());
        };

        let resume_l1_block = context
            .fetch_commit_block(pending_snapshot_l1_ref.commit_txid())
            .await?;
        context.save_reconstruction_snapshot(
            &anchor.replay_snapshot,
            &anchor.account_state,
            resume_l1_block,
            anchor.completion_block,
        )?;

        let next_update_seq_no = anchor.next_update_seq_no();
        if let Err(error) = context.prune_recovered_da_before(next_update_seq_no).await {
            warn!(
                %error,
                next_update_seq_no,
                "failed to prune recovered EE DA below the saved snapshot boundary"
            );
        }
        self.verified_state_anchor
            .as_mut()
            .expect("verified state anchor remains installed after saving")
            .pending_snapshot_l1_ref = None;
        info!(next_update_seq_no, "saved verified EE state snapshot");
        Ok(())
    }

    async fn fetch_finalized_ol_prefix(
        &self,
        context: &impl DaVerifierContext,
        mut recovered_blobs: Vec<RecoveredDaBlob>,
        first_unfinalized_update_seq_no_cache: &mut Option<Seqno>,
    ) -> Result<(Vec<RecoveredDaBlob>, Vec<OLAccountUpdate>), DaVerifierError> {
        let account_id = self.params.strata_exec_account_id();
        let anchor_next_update_seq_no = Seqno::new(self.next_update_seq_no());
        let finalized_next_update_seq_no = match *first_unfinalized_update_seq_no_cache {
            Some(sequence) => sequence,
            None => {
                let sequence = context
                    .fetch_finalized_next_update_seq_no(account_id)
                    .await
                    .map_err(
                        |source| VerifyAccountStateError::FetchFinalizedAccountState { source },
                    )?;
                *first_unfinalized_update_seq_no_cache = Some(sequence);
                sequence
            }
        };
        if finalized_next_update_seq_no < anchor_next_update_seq_no {
            return Err(DaVerifierError::LaggingFinalizedOLFrontier {
                anchor_next_update_seq_no,
                finalized_next_update_seq_no,
            });
        }

        let finalized_sequence = *finalized_next_update_seq_no.inner();
        let available_candidate_count = recovered_blobs
            .iter()
            .position(|recovered_blob| recovered_blob.blob().update_seq_no >= finalized_sequence)
            .unwrap_or(recovered_blobs.len());
        recovered_blobs.truncate(available_candidate_count);

        let mut account_updates = Vec::new();
        let mut previous_update_seq_no = None;
        for recovered_blob in &recovered_blobs {
            let update_seq_no = Seqno::new(recovered_blob.blob().update_seq_no);
            if previous_update_seq_no == Some(update_seq_no) {
                continue;
            }
            let update = context
                .fetch_account_update(account_id, update_seq_no)
                .await
                .map_err(|source| VerifyAccountStateError::FetchAccountUpdate {
                    update_seq_no,
                    source,
                })?;
            account_updates.push(update);
            previous_update_seq_no = Some(update_seq_no);
        }

        Ok((recovered_blobs, account_updates))
    }

    async fn load_contiguous_da_prefix(
        &self,
        context: &impl DaVerifierContext,
        recovered_l1_frontier: L1Height,
    ) -> Result<Vec<RecoveredDaBlob>, DaVerifierError> {
        let next_update_seq_no = self.next_update_seq_no();
        context
            .get_contiguous_recovered_da(next_update_seq_no, recovered_l1_frontier)
            .await
            .map_err(Into::into)
    }

    fn advance_verified_state_anchor(
        &mut self,
        l1_reconstruction_outcome: L1ReconstructionOutcome,
        verified_account_state: VerifiedAccountState,
        completion_block: L1BlockCommitment,
    ) -> Result<(), DaVerifierError> {
        let last_batch_l1_ref = l1_reconstruction_outcome.last_batch_l1_ref();
        let batch_replay_outcome = l1_reconstruction_outcome.batch_replay_outcome();
        let last_update_seq_no = batch_replay_outcome.applied_range().last_update_seq_no();
        let next_update_seq_no = last_update_seq_no
            .inner()
            .checked_add(1)
            .expect("batch replay rejects terminal update sequence numbers");
        let last_block_num = batch_replay_outcome.applied_range().last_block_num();
        let reconstructed_state_root = batch_replay_outcome.final_state_root();
        let reconstructed_inner_state_root =
            compute_ee_account_inner_root(verified_account_state.state());
        info!(
            last_update_seq_no = *last_update_seq_no.inner(),
            last_block_num,
            next_inbox_msg_idx = verified_account_state.next_inbox_msg_idx(),
            reconstructed_state_root = %reconstructed_state_root,
            ?reconstructed_inner_state_root,
            expected_inner_state_root = ?verified_account_state.expected_inner_state_root(),
            "verified reconstructed EE state against OL"
        );

        let batch_replay_outcome = l1_reconstruction_outcome.into_batch_replay_outcome();
        let replay_snapshot = BatchReplaySnapshot::try_new(
            Seqno::new(next_update_seq_no),
            last_block_num,
            reconstructed_state_root,
            batch_replay_outcome.into_final_state(),
        )
        .map_err(L1ReconstructionError::from)?;
        self.verified_state_anchor = Some(VerifiedStateAnchor {
            replay_snapshot,
            account_state: verified_account_state,
            pending_snapshot_l1_ref: Some(last_batch_l1_ref),
            completion_block,
        });
        Ok(())
    }
}

/// Tracks whether persisted verifier state has been resolved for this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotInitializationState {
    Pending,
    Complete,
}

/// Owns the verifier context and per-phase mutable state.
pub(crate) struct DaVerifierServiceState<C> {
    context: Arc<C>,
    recovery_state: DaRecoveryState,
    verification_state: DaVerificationState,

    /// Records that the next tick must retry verification before scanning another L1 window.
    retry_verification_before_scan: bool,
    /// Remains pending across ticks until persisted verifier state is initialized successfully.
    snapshot_initialization: SnapshotInitializationState,
}

impl<C: DaVerifierContext> DaVerifierServiceState<C> {
    /// Creates verifier state from its recovery and verification components.
    pub(crate) fn new(
        context: Arc<C>,
        recovery_state: DaRecoveryState,
        verification_state: DaVerificationState,
    ) -> Self {
        Self {
            context,
            recovery_state,
            verification_state,
            retry_verification_before_scan: false,
            snapshot_initialization: SnapshotInitializationState::Pending,
        }
    }

    /// Resolves persisted verifier progress once before recovery begins.
    async fn initialize_from_snapshot(&mut self) -> Result<(), DaVerifierError> {
        if self.snapshot_initialization == SnapshotInitializationState::Complete {
            return Ok(());
        }

        let Some(snapshot) = self.context.as_ref().load_reconstruction_snapshot()? else {
            self.context.as_ref().clear_recovered_da().await?;
            self.snapshot_initialization = SnapshotInitializationState::Complete;
            return Ok(());
        };

        let resume_l1_block = snapshot.resume_l1_block();
        let completion_block = snapshot.completion_block();
        if resume_l1_block.height() < self.recovery_state.genesis_l1_height {
            warn!(
                snapshot_resume_l1_height = resume_l1_block.height(),
                configured_genesis_l1_height = self.recovery_state.genesis_l1_height,
                "snapshot predates configured genesis; restarting EE DA verification from genesis"
            );
            return self.discard_snapshot_and_restart_from_genesis().await;
        }
        if !self
            .context
            .as_ref()
            .is_l1_block_canonical(resume_l1_block)
            .await?
        {
            warn!(
                %resume_l1_block,
                "snapshot scan cursor block is no longer canonical; restarting EE DA verification from genesis"
            );
            return self.discard_snapshot_and_restart_from_genesis().await;
        }
        if !self
            .context
            .as_ref()
            .is_l1_block_canonical(completion_block)
            .await?
        {
            warn!(
                %completion_block,
                "snapshot's highest DA completion block is no longer canonical; restarting EE DA verification from genesis"
            );
            return self.discard_snapshot_and_restart_from_genesis().await;
        }

        self.recovery_state
            .driver
            .set_next_l1_height(resume_l1_block.height());
        self.verification_state.restore_snapshot(snapshot);
        self.snapshot_initialization = SnapshotInitializationState::Complete;
        info!(
            resume_l1_height = resume_l1_block.height(),
            completion_l1_height = completion_block.height(),
            "restored canonical EE DA verification snapshot"
        );
        Ok(())
    }

    async fn discard_snapshot_and_restart_from_genesis(&mut self) -> Result<(), DaVerifierError> {
        self.context.as_ref().delete_reconstruction_snapshot()?;
        self.context.as_ref().clear_recovered_da().await?;
        self.snapshot_initialization = SnapshotInitializationState::Complete;
        Ok(())
    }

    /// Advances DA recovery and state verification as far as the current frontiers allow.
    pub(crate) async fn handle_tick(&mut self) -> Result<(), DaVerifierError> {
        // 1. Restore a usable snapshot or discard stale persisted state before recovery.
        self.initialize_from_snapshot().await?;

        // 2. Persist a verified anchor left pending by an earlier failed save.
        self.verification_state
            .save_verified_state_snapshot(self.context.as_ref())
            .await?;

        // 3. Decide whether this tick needs to scan reorg-safe L1 blocks.
        let recovery_resolution = self.resolve_da_recovery().await?;

        match recovery_resolution {
            DaRecoveryResolution::RecoverThrough { reorg_safe_tip } => {
                // 4. Recover DA through the safe tip and verify each reconstructed prefix.
                self.recover_da_and_verify_state(reorg_safe_tip).await?;
            }
            DaRecoveryResolution::VerifyRecoveredDa => {
                // 4. Even without new L1 data, verify DA already persisted by an earlier tick.
                self.reconstruct_and_verify_state_without_da_recovery()
                    .await?;
            }
        }

        // 5. Report the durable recovery and verified-state frontiers reached by this tick.
        self.log_completed_tick();
        Ok(())
    }

    async fn resolve_da_recovery(&mut self) -> Result<DaRecoveryResolution, DaVerifierError> {
        match self
            .recovery_state
            .recovery_action(self.context.as_ref())
            .await
        {
            Ok(DaRecoveryAction::RecoverThrough { reorg_safe_tip }) => {
                Ok(DaRecoveryResolution::RecoverThrough { reorg_safe_tip })
            }
            Ok(DaRecoveryAction::WaitForReorgSafeTip) => {
                debug!(
                    reorg_safe_depth = self.recovery_state.l1_reorg_safe_depth,
                    "Bitcoin chain has not reached the EE DA recovery depth"
                );
                Ok(DaRecoveryResolution::VerifyRecoveredDa)
            }
            Ok(DaRecoveryAction::CaughtUp { reorg_safe_tip }) => {
                debug!(
                    next_l1_height = self.recovery_state.driver.next_l1_height(),
                    reorg_safe_tip, "no reorg-safe L1 blocks require EE DA recovery"
                );
                Ok(DaRecoveryResolution::VerifyRecoveredDa)
            }
            Err(error) if error.is_recoverable() && !error.is_database_failure() => {
                warn!(%error, "EE DA recovery failed; verifying already recovered EE DA anyway");
                Ok(DaRecoveryResolution::VerifyRecoveredDa)
            }
            Err(error) => Err(error),
        }
    }

    /// Recovers DA through a frozen L1 tip and verifies every durable recovered prefix.
    async fn recover_da_and_verify_state(
        &mut self,
        reorg_safe_tip: L1Height,
    ) -> Result<(), DaVerifierError> {
        // Keep one finalized OL boundary for every reconstruction in this tick.
        let mut first_unfinalized_update_seq_no_cache = None;

        // OL may have bounded the previous tick. Retry persisted DA before scanning more L1.
        if self.retry_verification_before_scan {
            self.reconstruct_and_verify_state_through_current_recovered_l1_frontier(
                &mut first_unfinalized_update_seq_no_cache,
            )
            .await?;

            // Still bounded by OL after the retry, so scanning more L1 cannot help.
            if self.retry_verification_before_scan {
                return Ok(());
            }
        }

        while self.recovery_state.driver.next_l1_height() <= reorg_safe_tip {
            let completed_recovery = match self
                .recovery_state
                .scan_next_l1_window_and_persist_recovered_da(self.context.as_ref(), reorg_safe_tip)
                .await
            {
                Ok(completed_recovery) => completed_recovery,
                Err(error) if error.is_recoverable() && !error.is_database_failure() => {
                    warn!(%error, "EE DA recovery failed; verifying already recovered EE DA before ending the current tick");
                    self.reconstruct_and_verify_state_through_current_recovered_l1_frontier(
                        &mut first_unfinalized_update_seq_no_cache,
                    )
                    .await?;
                    break;
                }
                Err(error) => return Err(error),
            };

            self.reconstruct_and_verify_state_through_l1_frontier(
                completed_recovery.recovered_l1_frontier,
                &mut first_unfinalized_update_seq_no_cache,
            )
            .await?;
            self.log_completed_da_recovery_range(
                completed_recovery,
                reorg_safe_tip,
                first_unfinalized_update_seq_no_cache,
            );

            if self.retry_verification_before_scan {
                break;
            }
        }

        Ok(())
    }

    async fn reconstruct_and_verify_state_without_da_recovery(
        &mut self,
    ) -> Result<(), DaVerifierError> {
        let Some(recovered_l1_frontier) = self.recovery_state.recovered_l1_frontier() else {
            return Ok(());
        };

        let mut first_unfinalized_update_seq_no_cache = None;
        self.reconstruct_and_verify_state_through_l1_frontier(
            recovered_l1_frontier,
            &mut first_unfinalized_update_seq_no_cache,
        )
        .await
    }

    async fn reconstruct_and_verify_state_through_current_recovered_l1_frontier(
        &mut self,
        first_unfinalized_update_seq_no_cache: &mut Option<Seqno>,
    ) -> Result<(), DaVerifierError> {
        let Some(recovered_l1_frontier) = self.recovery_state.recovered_l1_frontier() else {
            return Ok(());
        };

        self.reconstruct_and_verify_state_through_l1_frontier(
            recovered_l1_frontier,
            first_unfinalized_update_seq_no_cache,
        )
        .await
    }

    async fn reconstruct_and_verify_state_through_l1_frontier(
        &mut self,
        recovered_l1_frontier: L1Height,
        first_unfinalized_update_seq_no_cache: &mut Option<Seqno>,
    ) -> Result<(), DaVerifierError> {
        self.verification_state
            .reconstruct_and_verify_state_from_recovered_da(
                self.context.as_ref(),
                recovered_l1_frontier,
                first_unfinalized_update_seq_no_cache,
            )
            .await?;
        self.retry_verification_before_scan =
            first_unfinalized_update_seq_no_cache.is_some_and(|frontier| {
                self.verification_state
                    .verification_reached_finalized_ol(frontier)
            });
        self.verification_state
            .save_verified_state_snapshot(self.context.as_ref())
            .await?;
        Ok(())
    }

    fn log_completed_da_recovery_range(
        &self,
        completed_recovery: CompletedDaRecoveryRange,
        reorg_safe_tip: L1Height,
        first_unfinalized_update_seq_no: Option<Seqno>,
    ) {
        let first_unfinalized_update_seq_no = first_unfinalized_update_seq_no
            .filter(|_| self.retry_verification_before_scan)
            .map(|sequence| *sequence.inner());
        info!(
            start_height = completed_recovery.start_l1_height,
            end_height = completed_recovery.recovered_l1_frontier,
            reorg_safe_tip,
            next_l1_height = self.recovery_state.driver.next_l1_height(),
            next_update_seq_no = self.verification_state.next_update_seq_no(),
            ?first_unfinalized_update_seq_no,
            "completed L1 range for EE DA recovery"
        );
    }

    fn log_completed_tick(&self) {
        info!(
            next_l1_height = self.recovery_state.driver.next_l1_height(),
            reorg_safe_tip = ?self.recovery_state.reorg_safe_tip,
            next_update_seq_no = self.verification_state.next_update_seq_no(),
            "completed EE DA verifier tick"
        );
    }

    /// Returns the next L1 height to process.
    pub(crate) fn next_l1_height(&self) -> L1Height {
        self.recovery_state.driver.next_l1_height()
    }

    /// Returns the latest reorg-safe L1 tip observed by recovery.
    pub(crate) fn reorg_safe_tip(&self) -> Option<L1Height> {
        self.recovery_state.reorg_safe_tip
    }

    /// Returns the next EE update sequence number verification expects.
    pub(crate) fn next_update_seq_no(&self) -> u64 {
        self.verification_state.next_update_seq_no()
    }
}

fn decide_da_recovery_action(
    tip_height: L1Height,
    reorg_safe_depth: u32,
    next_l1_height: L1Height,
) -> DaRecoveryAction {
    let Some(reorg_safe_tip) = tip_height.checked_sub(reorg_safe_depth) else {
        return DaRecoveryAction::WaitForReorgSafeTip;
    };

    if next_l1_height > reorg_safe_tip {
        DaRecoveryAction::CaughtUp { reorg_safe_tip }
    } else {
        DaRecoveryAction::RecoverThrough { reorg_safe_tip }
    }
}

fn l1_scan_window_end(
    start_height: L1Height,
    max_window_size: NonZeroU32,
    target_height: L1Height,
) -> L1Height {
    start_height
        .checked_add(max_window_size.get() - 1)
        .unwrap_or(L1Height::MAX)
        .min(target_height)
}

/// Computes the highest DA completion block covered by prior and current verified state.
fn compute_highest_completion_block(
    previous: Option<L1BlockCommitment>,
    current: impl IntoIterator<Item = L1BlockCommitment>,
) -> Option<L1BlockCommitment> {
    current
        .into_iter()
        .chain(previous)
        .max_by_key(|block| block.height())
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use bitcoin::{hashes::Hash as _, Txid};
    use bitcoind_async_client::error::ClientError;
    use strata_identifiers::{Buf32, L1BlockId};

    use super::*;
    use crate::account_state::OLAccountUpdateError;

    const SAFE_DEPTH: u32 = 6;
    const TIP: L1Height = 100;
    const SAFE_TIP: L1Height = TIP - SAFE_DEPTH;
    const TEST_SCAN_WINDOW_SIZE: NonZeroU32 = NonZeroU32::new(3).expect("3 is nonzero");

    fn build_l1_commitment(height: L1Height, block_id_byte: u8) -> L1BlockCommitment {
        L1BlockCommitment::new(height, L1BlockId::from(Buf32::from([block_id_byte; 32])))
    }

    #[test]
    fn test_recovery_waits_when_chain_has_no_reorg_safe_tip() {
        let action = decide_da_recovery_action(5, SAFE_DEPTH, 0);

        assert_eq!(action, DaRecoveryAction::WaitForReorgSafeTip);
        assert_eq!(action.reorg_safe_tip(), None);
    }

    #[test]
    fn test_recovery_is_caught_up_after_reorg_safe_tip() {
        let action = decide_da_recovery_action(TIP, SAFE_DEPTH, SAFE_TIP + 1);

        assert_eq!(
            action,
            DaRecoveryAction::CaughtUp {
                reorg_safe_tip: SAFE_TIP,
            }
        );
        assert_eq!(action.reorg_safe_tip(), Some(SAFE_TIP));
    }

    #[test]
    fn test_recovery_includes_next_height_at_reorg_safe_tip() {
        let action = decide_da_recovery_action(TIP, SAFE_DEPTH, SAFE_TIP);

        assert_eq!(
            action,
            DaRecoveryAction::RecoverThrough {
                reorg_safe_tip: SAFE_TIP,
            }
        );
    }

    #[test]
    fn test_l1_scan_window_uses_configured_size() {
        let end_height = l1_scan_window_end(10, TEST_SCAN_WINDOW_SIZE, 20);

        assert_eq!(end_height, 12);
    }

    #[test]
    fn test_l1_scan_window_is_truncated_at_target() {
        let end_height = l1_scan_window_end(19, TEST_SCAN_WINDOW_SIZE, 20);

        assert_eq!(end_height, 20);
    }

    #[test]
    fn test_l1_scan_window_handles_terminal_height() {
        let end_height =
            l1_scan_window_end(L1Height::MAX - 1, TEST_SCAN_WINDOW_SIZE, L1Height::MAX);

        assert_eq!(end_height, L1Height::MAX);
    }

    #[test]
    fn test_highest_completion_block_uses_current_window_maximum() {
        let expected = build_l1_commitment(12, 2);

        let actual = compute_highest_completion_block(
            Some(build_l1_commitment(10, 0)),
            [build_l1_commitment(11, 1), expected],
        );

        assert_eq!(actual, Some(expected));
    }

    #[test]
    fn test_highest_completion_block_uses_current_window_without_previous_anchor() {
        let expected = build_l1_commitment(12, 2);

        let actual = compute_highest_completion_block(None, [build_l1_commitment(11, 1), expected]);

        assert_eq!(actual, Some(expected));
    }

    #[test]
    fn test_highest_completion_block_preserves_previous_maximum() {
        let previous = build_l1_commitment(12, 2);

        let actual = compute_highest_completion_block(
            Some(previous),
            [build_l1_commitment(10, 0), build_l1_commitment(11, 1)],
        );

        assert_eq!(actual, Some(previous));
    }

    #[test]
    fn test_recoverable_bitcoin_tip_failure_is_forwarded() {
        let error =
            DaVerifierError::FetchBitcoinTip(FetchBitcoinTipError::Rpc(ClientError::Timeout));

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_recoverable_snapshot_load_failure_is_forwarded() {
        let error = DaVerifierError::LoadSnapshot(SnapshotLoadError::Io {
            path: PathBuf::from("snapshot"),
            source: io::Error::other("test snapshot read failure"),
        });

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_recoverable_snapshot_delete_failure_is_forwarded() {
        let error = DaVerifierError::DeleteSnapshot(SnapshotDeleteError::Io {
            operation: "remove reconstruction snapshot",
            path: PathBuf::from("snapshot"),
            source: io::Error::other("test snapshot delete failure"),
        });

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_recoverable_snapshot_canonicality_failure_is_forwarded() {
        let error = DaVerifierError::CheckL1Block(CheckL1BlockError::FetchBlockHash {
            expected: L1BlockCommitment::default(),
            source: ClientError::Timeout,
        });

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_recoverable_database_failure_is_forwarded() {
        let error = DaVerifierError::RecoveredDaDb(RecoveredDaDbError::WorkerCancelled);

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_fatal_database_failure_is_forwarded() {
        let error =
            DaVerifierError::RecoveredDaDb(RecoveredDaDbError::WorkerPanicked("panic".to_owned()));

        assert!(!error.is_recoverable());
    }

    fn account_update_failure(recoverable: bool) -> DaVerifierError {
        DaVerifierError::VerifyAccountState(VerifyAccountStateError::FetchAccountUpdate {
            update_seq_no: Seqno::zero(),
            source: OLAccountUpdateError::new(
                io::Error::other("test account update failure"),
                recoverable,
            ),
        })
    }

    fn finalized_account_state_failure(recoverable: bool) -> DaVerifierError {
        DaVerifierError::VerifyAccountState(VerifyAccountStateError::FetchFinalizedAccountState {
            source: OLAccountUpdateError::new(
                io::Error::other("test finalized account state failure"),
                recoverable,
            ),
        })
    }

    #[test]
    fn test_recoverable_account_update_failure_is_forwarded() {
        let error = account_update_failure(true);

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_fatal_account_update_failure_is_forwarded() {
        let error = account_update_failure(false);

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_recoverable_finalized_account_state_failure_is_forwarded() {
        let error = finalized_account_state_failure(true);

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_fatal_finalized_account_state_failure_is_forwarded() {
        let error = finalized_account_state_failure(false);

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_fatal_recovery_failure_is_forwarded() {
        let error = DaVerifierError::DaRecovery(DaRecoveryError::NonContiguousBlocks {
            expected: 42,
            actual: 43,
        });

        assert!(!error.is_recoverable());
    }

    #[test]
    fn test_recoverable_snapshot_save_failure_is_forwarded() {
        let error = DaVerifierError::SaveSnapshot(SnapshotSaveError::Io {
            operation: "test snapshot operation",
            path: PathBuf::from("snapshot"),
            source: io::Error::from_raw_os_error(28),
        });

        assert!(error.is_recoverable());
    }

    #[test]
    fn test_recoverable_commit_block_failure_is_forwarded() {
        let error = DaVerifierError::FetchCommitBlock(FetchCommitBlockError::Unconfirmed {
            txid: Txid::all_zeros(),
        });

        assert!(error.is_recoverable());
    }
}
