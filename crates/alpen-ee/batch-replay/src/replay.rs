//! Replays ordered batches against Ethereum state.

use alloy_primitives::Address;
use alpen_ee_da_types::EvmHeaderSummary;
use alpen_reth_statediff::{
    apply_batch_state_diff_to_ethereum_state, ethereum_state_from_genesis_accounts, BatchStateDiff,
    EthereumStateExt, GenesisAccount,
};
use rsp_mpt::EthereumState;
use strata_identifiers::Buf32;
use strata_snark_acct_types::Seqno;

use crate::{BatchReplayError, BatchReplaySnapshot};

/// EVM genesis is block 0; the first block applied from DA is block 1.
const GENESIS_LAST_APPLIED_BLOCK_NUM: u64 = 0;

/// Genesis replay expects the first batch at update_seq_no 0.
const GENESIS_FIRST_UPDATE_SEQ_NO: Seqno = Seqno::zero();

/// Source-neutral EVM batch replay input.
#[derive(Clone, Debug)]
pub struct EvmReplayBatch {
    update_seq_no: Seqno,
    evm_header: EvmHeaderSummary,
    state_diff: BatchStateDiff,
}

impl EvmReplayBatch {
    /// Creates a replay batch from decoded EVM state-diff data.
    pub fn new(
        update_seq_no: Seqno,
        evm_header: EvmHeaderSummary,
        state_diff: BatchStateDiff,
    ) -> Self {
        Self {
            update_seq_no,
            evm_header,
            state_diff,
        }
    }

    /// Returns the monotonic EE account update sequence number for this batch.
    pub fn update_seq_no(&self) -> Seqno {
        self.update_seq_no
    }

    /// Returns the EVM header context of the last block in this batch.
    pub fn evm_header(&self) -> &EvmHeaderSummary {
        &self.evm_header
    }

    /// Returns the aggregated state diff applied by this batch.
    pub fn state_diff(&self) -> &BatchStateDiff {
        &self.state_diff
    }
}

/// State root produced by one applied replay batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedBatchRoot {
    update_seq_no: Seqno,
    evm_header: EvmHeaderSummary,
    post_state_root: Buf32,
}

impl AppliedBatchRoot {
    fn new(update_seq_no: Seqno, evm_header: EvmHeaderSummary, post_state_root: Buf32) -> Self {
        Self {
            update_seq_no,
            evm_header,
            post_state_root,
        }
    }

    /// Returns the update sequence number that was applied.
    pub fn update_seq_no(&self) -> Seqno {
        self.update_seq_no
    }

    /// Returns the EVM header context carried by the applied batch.
    pub fn evm_header(&self) -> &EvmHeaderSummary {
        &self.evm_header
    }

    /// Returns the Ethereum state root after applying the batch.
    pub fn post_state_root(&self) -> Buf32 {
        self.post_state_root
    }
}

/// Inclusive range covered by a replay run that applied at least one batch.
///
/// The EVM block range covers all blocks after the replay anchor through the
/// final batch endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedBatchRange {
    first_update_seq_no: Seqno,
    last_update_seq_no: Seqno,
    first_block_num: u64,
    last_block_num: u64,
}

impl AppliedBatchRange {
    fn from_applied_roots(
        first_block_num: u64,
        applied_roots: &[AppliedBatchRoot],
    ) -> Option<Self> {
        let (Some(first), Some(last)) = (applied_roots.first(), applied_roots.last()) else {
            return None;
        };

        Some(Self {
            first_update_seq_no: first.update_seq_no,
            last_update_seq_no: last.update_seq_no,
            first_block_num,
            last_block_num: last.evm_header.block_num,
        })
    }

    /// Returns the first applied update sequence number.
    pub fn first_update_seq_no(&self) -> Seqno {
        self.first_update_seq_no
    }

    /// Returns the last applied update sequence number.
    pub fn last_update_seq_no(&self) -> Seqno {
        self.last_update_seq_no
    }

    /// Returns the first replayed EVM block number.
    pub fn first_block_num(&self) -> u64 {
        self.first_block_num
    }

    /// Returns the last replayed EVM block number.
    pub fn last_block_num(&self) -> u64 {
        self.last_block_num
    }
}

/// Successful output from replaying an ordered batch sequence.
#[derive(Debug)]
pub struct BatchReplayOutcome {
    final_state: EthereumState,
    applied_range: AppliedBatchRange,
    applied_roots: Vec<AppliedBatchRoot>,
}

impl BatchReplayOutcome {
    fn new(
        final_state: EthereumState,
        applied_range: AppliedBatchRange,
        applied_roots: Vec<AppliedBatchRoot>,
    ) -> Self {
        Self {
            final_state,
            applied_range,
            applied_roots,
        }
    }

    /// Returns the final Ethereum state root.
    pub fn final_state_root(&self) -> Buf32 {
        self.final_state.state_root_buf32()
    }

    /// Returns the inclusive range applied by this replay run.
    pub fn applied_range(&self) -> &AppliedBatchRange {
        &self.applied_range
    }

    /// Returns per-batch post-apply state roots.
    pub fn applied_roots(&self) -> &[AppliedBatchRoot] {
        &self.applied_roots
    }

    /// Returns the final Ethereum state.
    pub fn final_state(&self) -> &EthereumState {
        &self.final_state
    }

    /// Consumes the result and returns the final Ethereum state.
    pub fn into_final_state(self) -> EthereumState {
        self.final_state
    }
}

/// Replays ordered batches starting from explicit genesis accounts.
pub fn replay_from_genesis<A, I>(
    genesis_accounts: A,
    batches: I,
) -> Result<BatchReplayOutcome, BatchReplayError>
where
    A: IntoIterator<Item = (Address, GenesisAccount)>,
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let state = ethereum_state_from_genesis_accounts(genesis_accounts)
        .map_err(|source| BatchReplayError::GenesisState { source })?;
    replay_from_state(
        state,
        GENESIS_FIRST_UPDATE_SEQ_NO,
        GENESIS_LAST_APPLIED_BLOCK_NUM,
        batches,
    )
}

/// Replays ordered batches starting from a validated state snapshot.
pub fn replay_from_snapshot<I>(
    snapshot: BatchReplaySnapshot,
    batches: I,
) -> Result<BatchReplayOutcome, BatchReplayError>
where
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let (next_update_seq_no, last_applied_block_num, state) = snapshot.into_parts();
    replay_from_state(state, next_update_seq_no, last_applied_block_num, batches)
}

fn replay_from_state<I>(
    mut state: EthereumState,
    anchor_update_seq_no: Seqno,
    anchor_block_num: u64,
    batches: I,
) -> Result<BatchReplayOutcome, BatchReplayError>
where
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let mut batches = batches.into_iter().peekable();
    if batches.peek().is_none() {
        return Err(BatchReplayError::NoBatches);
    }

    let mut next_update_seq_no = anchor_update_seq_no;
    let mut previous_block_num = anchor_block_num;
    let mut applied_roots = Vec::new();

    for batch in batches {
        let update_seq_no = batch.update_seq_no();
        let evm_header = *batch.evm_header();

        if update_seq_no != next_update_seq_no {
            return Err(BatchReplayError::UnexpectedSeqNo {
                expected: next_update_seq_no,
                actual: update_seq_no,
            });
        }
        if evm_header.block_num <= previous_block_num {
            return Err(BatchReplayError::BlockContinuityViolation {
                update_seq_no,
                expected_after_block_num: previous_block_num,
                actual_block_num: evm_header.block_num,
            });
        }
        let following_update_seq_no = update_seq_no
            .inner()
            .checked_add(1)
            .map(Seqno::new)
            .ok_or(BatchReplayError::TerminalUpdateSeqNo { update_seq_no })?;

        apply_batch_state_diff_to_ethereum_state(&mut state, batch.state_diff())?;
        let post_state_root = state.state_root_buf32();
        previous_block_num = evm_header.block_num;
        next_update_seq_no = following_update_seq_no;
        applied_roots.push(AppliedBatchRoot::new(
            update_seq_no,
            evm_header,
            post_state_root,
        ));
    }

    let first_block_num = anchor_block_num
        .checked_add(1)
        .expect("successful replay advanced past the block anchor");
    let applied_range = AppliedBatchRange::from_applied_roots(first_block_num, &applied_roots)
        .expect("non-empty replay produces applied roots");
    Ok(BatchReplayOutcome::new(state, applied_range, applied_roots))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{B256, U256};
    use alpen_ee_da_types::EvmHeaderSummary;
    use alpen_reth_statediff::{
        ethereum_state_from_genesis_accounts,
        test_utils::{
            account_change, addr, batch_diff, block_diff, hash, slot, snapshot, storage_change,
            value,
        },
        BatchStateDiff, EthereumStateExt, GenesisAccount,
    };
    use rsp_mpt::EthereumState;

    use super::*;

    fn make_empty_ethereum_state() -> EthereumState {
        ethereum_state_from_genesis_accounts(Vec::<(Address, GenesisAccount)>::new())
            .expect("empty genesis state builds")
    }

    fn build_genesis_account(balance: u64, nonce: u64) -> GenesisAccount {
        GenesisAccount {
            nonce: Some(nonce),
            balance: U256::from(balance),
            code: None,
            storage: Some(BTreeMap::new()),
            private_key: None,
        }
    }

    fn build_genesis_account_with_storage(slot_key: U256, slot_value: U256) -> GenesisAccount {
        GenesisAccount {
            nonce: Some(0),
            balance: U256::ZERO,
            code: None,
            storage: Some(BTreeMap::from([(
                B256::from(slot_key.to_be_bytes::<32>()),
                B256::from(slot_value.to_be_bytes::<32>()),
            )])),
            private_key: None,
        }
    }

    fn build_evm_header(block_num: u64) -> EvmHeaderSummary {
        EvmHeaderSummary {
            block_num,
            timestamp: 1_700_000_000 + block_num,
            base_fee: 100,
            gas_used: 21_000,
            gas_limit: 36_000_000,
        }
    }

    fn make_account_creation_diff(seed: u8) -> BatchStateDiff {
        let mut block = block_diff();
        account_change(
            &mut block,
            addr(seed),
            None,
            Some(snapshot(seed as u64, 1, hash(seed))),
        );
        batch_diff(&[block])
    }

    fn make_account_creation_diff_with_storage(seed: u8) -> BatchStateDiff {
        let address = addr(seed);
        let mut block = block_diff();
        account_change(
            &mut block,
            address,
            None,
            Some(snapshot(seed as u64, 1, hash(seed))),
        );
        storage_change(&mut block, address, slot(1), value(0), value(42));
        batch_diff(&[block])
    }

    fn make_storage_update_diff(seed: u8) -> BatchStateDiff {
        let mut block = block_diff();
        storage_change(&mut block, addr(seed), slot(1), value(10), value(11));
        batch_diff(&[block])
    }

    fn build_batch(update_seq_no: u64, block_num: u64, seed: u8) -> EvmReplayBatch {
        EvmReplayBatch::new(
            Seqno::new(update_seq_no),
            build_evm_header(block_num),
            make_account_creation_diff(seed),
        )
    }

    #[test]
    fn test_ordered_batches_produce_final_root() {
        let batches = vec![
            build_batch(0, 3, 0x10),
            build_batch(1, 6, 0x11),
            build_batch(2, 9, 0x12),
        ];

        let result = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), batches)
            .expect("ordered replay succeeds");

        assert_eq!(result.applied_roots().len(), 3);
        assert_eq!(
            result.final_state_root(),
            result.applied_roots()[2].post_state_root()
        );
        assert_eq!(
            result.final_state().state_root_buf32(),
            result.final_state_root()
        );
        let applied_range = result.applied_range();
        assert_eq!(applied_range.first_update_seq_no(), Seqno::new(0));
        assert_eq!(applied_range.last_update_seq_no(), Seqno::new(2));
        assert_eq!(applied_range.first_block_num(), 1);
        assert_eq!(applied_range.last_block_num(), 9);
    }

    #[test]
    fn test_non_empty_diff_updates_account_and_storage() {
        let address = addr(0x11);
        let batch = EvmReplayBatch::new(
            Seqno::zero(),
            build_evm_header(3),
            make_account_creation_diff_with_storage(0x11),
        );

        let result = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), [batch])
            .expect("replay succeeds");

        assert_eq!(result.applied_roots().len(), 1);
        assert_eq!(
            result.final_state().get_account_snapshot(address).unwrap(),
            Some(snapshot(0x11, 1, hash(0x11)))
        );
        assert_eq!(
            result
                .final_state()
                .get_storage_slot(address, slot(1))
                .unwrap(),
            value(42)
        );
    }

    #[test]
    fn test_empty_input_returns_error() {
        let err = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), Vec::new())
            .expect_err("empty replay rejects");

        assert!(matches!(err, BatchReplayError::NoBatches));
    }

    #[test]
    fn test_invalid_state_diff_returns_error() {
        let address = addr(0x11);
        let mut state = ethereum_state_from_genesis_accounts([(
            address,
            build_genesis_account_with_storage(slot(1), value(10)),
        )])
        .expect("genesis state builds");
        // Force an incomplete sparse witness so the state-diff applier rejects it.
        state.storage_tries.clear();
        let state_root = state.state_root_buf32();
        let snapshot = BatchReplaySnapshot::try_new(Seqno::new(8), 10, state_root, state)
            .expect("snapshot root matches");
        let batch = EvmReplayBatch::new(
            Seqno::new(8),
            build_evm_header(11),
            make_storage_update_diff(0x11),
        );

        let err = replay_from_snapshot(snapshot, [batch]).expect_err("state diff rejects");

        assert!(matches!(err, BatchReplayError::ApplyDiff(_)));
    }

    #[test]
    fn test_snapshot_anchor_sets_applied_range() {
        let snapshot_state =
            ethereum_state_from_genesis_accounts([(addr(0x01), build_genesis_account(100, 1))])
                .expect("snapshot state builds");
        let snapshot_root = snapshot_state.state_root_buf32();
        let snapshot =
            BatchReplaySnapshot::try_new(Seqno::new(5), 10, snapshot_root, snapshot_state)
                .expect("snapshot root matches");
        assert_eq!(snapshot.state_root(), snapshot_root);
        let batches = vec![build_batch(5, 11, 0x20), build_batch(6, 14, 0x21)];

        let result = replay_from_snapshot(snapshot, batches).expect("snapshot replay succeeds");

        assert_eq!(result.applied_roots().len(), 2);
        let applied_range = result.applied_range();
        assert_eq!(applied_range.first_update_seq_no(), Seqno::new(5));
        assert_eq!(applied_range.last_update_seq_no(), Seqno::new(6));
        assert_eq!(applied_range.first_block_num(), 11);
        assert_eq!(applied_range.last_block_num(), 14);
    }

    #[test]
    fn test_snapshot_root_mismatch_returns_error() {
        let state = make_empty_ethereum_state();
        let expected = Buf32::from([1; 32]);
        let actual = state.state_root_buf32();

        let err = BatchReplaySnapshot::try_new(Seqno::new(5), 10, expected, state)
            .expect_err("mismatched snapshot root rejects");

        assert!(matches!(
            err,
            BatchReplayError::SnapshotRootMismatch {
                expected: error_expected,
                actual: error_actual,
            } if error_expected == expected && error_actual == actual
        ));
    }

    #[test]
    fn test_update_seqno_gap_returns_error() {
        let batches = vec![build_batch(0, 3, 0x10), build_batch(2, 6, 0x11)];

        let err = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), batches)
            .expect_err("gap rejects");

        assert!(matches!(
            err,
            BatchReplayError::UnexpectedSeqNo {
                expected,
                actual,
            } if expected == Seqno::new(1) && actual == Seqno::new(2)
        ));
    }

    #[test]
    fn test_non_increasing_block_number_returns_error() {
        let batches = vec![build_batch(0, 3, 0x10), build_batch(1, 3, 0x11)];

        let err = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), batches)
            .expect_err("non-increasing block rejects");

        assert!(matches!(
            err,
            BatchReplayError::BlockContinuityViolation {
                update_seq_no,
                expected_after_block_num: 3,
                actual_block_num: 3,
            } if update_seq_no == Seqno::new(1)
        ));
    }

    #[test]
    fn test_terminal_update_seqno_returns_error() {
        let batches = vec![build_batch(u64::MAX, 3, 0x10)];
        let state = make_empty_ethereum_state();
        let state_root = state.state_root_buf32();
        let snapshot = BatchReplaySnapshot::try_new(Seqno::new(u64::MAX), 2, state_root, state)
            .expect("snapshot root matches");

        let err = replay_from_snapshot(snapshot, batches).expect_err("terminal seqno rejects");

        assert!(matches!(
            err,
            BatchReplayError::TerminalUpdateSeqNo {
                update_seq_no,
            } if update_seq_no == Seqno::new(u64::MAX)
        ));
    }
}
