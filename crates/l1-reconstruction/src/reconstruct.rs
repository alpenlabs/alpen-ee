//! Reconstructs EVM state from EE DA blobs recovered on L1.

use alloy_primitives::Address;
use alpen_batch_replay::{
    replay_from_genesis, replay_from_snapshot, BatchReplayError, BatchReplayOutcome,
    BatchReplaySnapshot, EvmReplayBatch,
};
use alpen_da_l1_extraction::{DaL1Ref, RecoveredDaBlob};
use alpen_reth_statediff::GenesisAccount;
use strata_snark_acct_types::Seqno;
use thiserror::Error;

use crate::{build_ordered_replay_batches, BatchSequenceError};

/// Failure to reconstruct EVM state from recovered EE DA blobs.
#[derive(Debug, Error)]
pub enum L1ReconstructionError {
    /// The recovered blobs do not form a valid batch sequence.
    #[error(transparent)]
    BatchSequence(#[from] BatchSequenceError),

    /// Replaying the ordered batches failed.
    #[error(transparent)]
    BatchReplay(#[from] BatchReplayError),
}

/// Successful EVM reconstruction from EE DA blobs recovered on L1.
#[derive(Debug)]
pub struct L1ReconstructionOutcome {
    batch_replay_outcome: BatchReplayOutcome,
    first_batch_l1_ref: DaL1Ref,
    last_batch_l1_ref: DaL1Ref,
}

impl L1ReconstructionOutcome {
    fn new(
        batch_replay_outcome: BatchReplayOutcome,
        first_batch_l1_ref: DaL1Ref,
        last_batch_l1_ref: DaL1Ref,
    ) -> Self {
        Self {
            batch_replay_outcome,
            first_batch_l1_ref,
            last_batch_l1_ref,
        }
    }

    /// Returns the source-neutral batch replay outcome.
    pub const fn batch_replay_outcome(&self) -> &BatchReplayOutcome {
        &self.batch_replay_outcome
    }

    /// Returns the L1 reference for the first applied batch.
    pub const fn first_batch_l1_ref(&self) -> DaL1Ref {
        self.first_batch_l1_ref
    }

    /// Returns the L1 reference for the last applied batch.
    pub const fn last_batch_l1_ref(&self) -> DaL1Ref {
        self.last_batch_l1_ref
    }

    /// Consumes this value and returns the source-neutral batch replay outcome.
    pub fn into_batch_replay_outcome(self) -> BatchReplayOutcome {
        self.batch_replay_outcome
    }
}

/// Reconstructs EVM state from genesis using EE DA blobs recovered on L1.
///
/// Returns [`None`] when no blobs are supplied.
pub fn reconstruct_from_genesis<A, I>(
    genesis_accounts: A,
    recovered_blobs: I,
) -> Result<Option<L1ReconstructionOutcome>, L1ReconstructionError>
where
    A: IntoIterator<Item = (Address, GenesisAccount)>,
    I: IntoIterator<Item = RecoveredDaBlob>,
{
    reconstruct_from_blobs(recovered_blobs, |batches| {
        replay_from_genesis(genesis_accounts, batches)
    })
}

/// Reconstructs EVM state from a snapshot using new EE DA blobs recovered on L1.
///
/// Blobs below the snapshot's next update sequence number are already reflected
/// in the snapshot and are ignored. Returns [`None`] when no new blobs remain.
pub fn reconstruct_from_snapshot<I>(
    snapshot: BatchReplaySnapshot,
    recovered_blobs: I,
) -> Result<Option<L1ReconstructionOutcome>, L1ReconstructionError>
where
    I: IntoIterator<Item = RecoveredDaBlob>,
{
    let next_update_seq_no = *snapshot.next_update_seq_no().inner();
    let replay_blobs = recovered_blobs
        .into_iter()
        .filter(|recovered| recovered.blob().update_seq_no >= next_update_seq_no);
    reconstruct_from_blobs(replay_blobs, |batches| {
        replay_from_snapshot(snapshot, batches)
    })
}

fn reconstruct_from_blobs<I, F>(
    recovered_blobs: I,
    replay: F,
) -> Result<Option<L1ReconstructionOutcome>, L1ReconstructionError>
where
    I: IntoIterator<Item = RecoveredDaBlob>,
    F: FnOnce(Vec<EvmReplayBatch>) -> Result<BatchReplayOutcome, BatchReplayError>,
{
    let recovered_blobs = recovered_blobs.into_iter().collect::<Vec<_>>();
    if recovered_blobs.is_empty() {
        return Ok(None);
    }

    let l1_refs_by_update_seq_no = recovered_blobs
        .iter()
        .map(|recovered| {
            (
                Seqno::new(recovered.blob().update_seq_no),
                recovered.l1_ref(),
            )
        })
        .collect::<Vec<_>>();
    let blobs = recovered_blobs.into_iter().map(RecoveredDaBlob::into_blob);
    let batches = build_ordered_replay_batches(blobs)?;
    let batch_replay_outcome = replay(batches)?;
    let applied_range = batch_replay_outcome.applied_range();
    let first_batch_l1_ref = find_l1_ref_by_update_seq_no(
        &l1_refs_by_update_seq_no,
        applied_range.first_update_seq_no(),
    )
    .expect("the first applied replay batch originates from a recovered EE DA blob");
    let last_batch_l1_ref = find_l1_ref_by_update_seq_no(
        &l1_refs_by_update_seq_no,
        applied_range.last_update_seq_no(),
    )
    .expect("the last applied replay batch originates from a recovered EE DA blob");

    Ok(Some(L1ReconstructionOutcome::new(
        batch_replay_outcome,
        first_batch_l1_ref,
        last_batch_l1_ref,
    )))
}

fn find_l1_ref_by_update_seq_no(
    l1_refs_by_update_seq_no: &[(Seqno, DaL1Ref)],
    update_seq_no: Seqno,
) -> Option<DaL1Ref> {
    l1_refs_by_update_seq_no
        .iter()
        .find_map(|(candidate_seq_no, l1_ref)| {
            (*candidate_seq_no == update_seq_no).then_some(*l1_ref)
        })
}

#[cfg(test)]
mod tests {
    use alpen_batch_replay::{BatchReplayError, EvmReplayBatch};
    use alpen_reth_statediff::BatchStateDiff;

    use super::*;
    use crate::test_utils::{build_evm_header, build_recovered_blob, make_l1_ref};

    fn build_replay_batch(update_seq_no: u64, block_num: u64) -> EvmReplayBatch {
        EvmReplayBatch::new(
            Seqno::new(update_seq_no),
            build_evm_header(block_num),
            BatchStateDiff::new(),
        )
    }

    fn build_replay_snapshot<I>(batches: I) -> BatchReplaySnapshot
    where
        I: IntoIterator<Item = EvmReplayBatch>,
    {
        let outcome = replay_from_genesis([], batches).expect("snapshot batches replay");
        let applied_range = outcome.applied_range();
        let next_update_seq_no = applied_range
            .last_update_seq_no()
            .inner()
            .checked_add(1)
            .map(Seqno::new)
            .expect("snapshot fixture does not use terminal update sequence number");
        let last_applied_block_num = applied_range.last_block_num();
        let state_root = outcome.final_state_root();

        BatchReplaySnapshot::try_new(
            next_update_seq_no,
            last_applied_block_num,
            state_root,
            outcome.into_final_state(),
        )
        .expect("replay snapshot builds")
    }

    #[test]
    fn test_no_blobs_from_genesis() {
        let outcome = reconstruct_from_genesis([], Vec::new())
            .expect("empty genesis reconstruction succeeds");

        assert!(outcome.is_none());
    }

    #[test]
    fn test_reconstruct_from_genesis() {
        let outcome = reconstruct_from_genesis(
            [],
            [
                build_recovered_blob(1, 2, 0x11),
                build_recovered_blob(0, 1, 0x10),
            ],
        )
        .expect("genesis reconstruction succeeds")
        .expect("new blobs produce replay output");

        assert_eq!(
            outcome
                .batch_replay_outcome()
                .applied_range()
                .first_update_seq_no(),
            Seqno::zero()
        );
        assert_eq!(
            outcome
                .batch_replay_outcome()
                .applied_range()
                .last_update_seq_no(),
            Seqno::new(1)
        );
        assert_eq!(outcome.first_batch_l1_ref(), make_l1_ref(0x10));
        assert_eq!(outcome.last_batch_l1_ref(), make_l1_ref(0x11));
    }

    #[test]
    fn test_blob_already_applied_to_snapshot() {
        let outcome = reconstruct_from_snapshot(
            build_replay_snapshot([build_replay_batch(0, 1)]),
            [build_recovered_blob(0, 1, 0x10)],
        )
        .expect("fully overlapping reconstruction succeeds");

        assert!(outcome.is_none());
    }

    #[test]
    fn test_reconstruct_from_snapshot_skips_applied_blobs() {
        let outcome = reconstruct_from_snapshot(
            build_replay_snapshot([build_replay_batch(0, 1)]),
            [
                build_recovered_blob(0, 1, 0x10),
                build_recovered_blob(1, 2, 0x11),
                build_recovered_blob(2, 3, 0x12),
            ],
        )
        .expect("snapshot reconstruction succeeds")
        .expect("new blobs produce replay output");

        assert_eq!(
            outcome
                .batch_replay_outcome()
                .applied_range()
                .first_update_seq_no(),
            Seqno::new(1)
        );
        assert_eq!(
            outcome
                .batch_replay_outcome()
                .applied_range()
                .last_update_seq_no(),
            Seqno::new(2)
        );
        assert_eq!(outcome.first_batch_l1_ref(), make_l1_ref(0x11));
        assert_eq!(outcome.last_batch_l1_ref(), make_l1_ref(0x12));
    }

    #[test]
    fn test_duplicate_at_snapshot_anchor_rejected() {
        let error = reconstruct_from_snapshot(
            build_replay_snapshot([build_replay_batch(0, 1)]),
            [
                build_recovered_blob(1, 2, 0x11),
                build_recovered_blob(1, 3, 0x12),
            ],
        )
        .expect_err("duplicate at snapshot anchor must fail");

        assert!(matches!(
            error,
            L1ReconstructionError::BatchSequence(BatchSequenceError::DuplicateUpdateSeqNo {
                update_seq_no,
            }) if update_seq_no == Seqno::new(1)
        ));
    }

    #[test]
    fn test_gap_at_snapshot_anchor_rejected() {
        let error = reconstruct_from_snapshot(
            build_replay_snapshot([build_replay_batch(0, 1)]),
            [build_recovered_blob(2, 3, 0x12)],
        )
        .expect_err("missing snapshot anchor batch must fail");

        assert!(matches!(
            error,
            L1ReconstructionError::BatchReplay(BatchReplayError::UnexpectedSeqNo {
                expected,
                actual,
            })
                if expected == Seqno::new(1) && actual == Seqno::new(2)
        ));
    }
}
