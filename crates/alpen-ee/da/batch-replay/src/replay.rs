//! Iterator-based replay choreography.

use alloy_primitives::Address;
use alpen_ee_da_types::EvmHeaderSummary;
use alpen_reth_statediff::{
    apply_batch_state_diff_to_ethereum_state, ethereum_state_from_genesis_accounts, BatchStateDiff,
    EthereumStateExt, GenesisAccount,
};
use rsp_mpt::EthereumState;
use strata_identifiers::Buf32;

use crate::{ReplayError, ReplayStateSnapshot};

/// EVM genesis is block 0; the first replayed block from DA is block 1.
const GENESIS_LAST_APPLIED_BLOCK_NUM: u64 = 0;

/// Genesis replay expects the first replay batch at update_seq_no 0.
const GENESIS_FIRST_UPDATE_SEQ_NO: u64 = 0;

/// Source-neutral EVM batch replay input.
#[derive(Clone, Debug)]
pub struct EvmReplayBatch {
    update_seq_no: u64,
    evm_header: EvmHeaderSummary,
    state_diff: BatchStateDiff,
}

impl EvmReplayBatch {
    /// Creates a replay batch from decoded EVM state-diff data.
    pub fn new(
        update_seq_no: u64,
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
    pub fn update_seq_no(&self) -> u64 {
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
    update_seq_no: u64,
    evm_header: EvmHeaderSummary,
    post_state_root: Buf32,
}

impl AppliedBatchRoot {
    fn new(update_seq_no: u64, evm_header: EvmHeaderSummary, post_state_root: Buf32) -> Self {
        Self {
            update_seq_no,
            evm_header,
            post_state_root,
        }
    }

    /// Returns the update sequence number that was applied.
    pub fn update_seq_no(&self) -> u64 {
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedRange {
    first_update_seq_no: u64,
    last_update_seq_no: u64,
    first_block_num: Option<u64>,
    last_block_num: u64,
}

impl AppliedRange {
    fn from_applied_roots(
        first_block_num: Option<u64>,
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
    pub fn first_update_seq_no(&self) -> u64 {
        self.first_update_seq_no
    }

    /// Returns the last applied update sequence number.
    pub fn last_update_seq_no(&self) -> u64 {
        self.last_update_seq_no
    }

    /// Returns the first replayed EVM block number if the starting anchor is known.
    pub fn first_block_num(&self) -> Option<u64> {
        self.first_block_num
    }

    /// Returns the last replayed EVM block number.
    pub fn last_block_num(&self) -> u64 {
        self.last_block_num
    }
}

/// Successful output from applying an ordered replay batch sequence.
#[derive(Debug)]
pub struct ReplayOutcome {
    final_state: EthereumState,
    applied_range: Option<AppliedRange>,
    applied_roots: Vec<AppliedBatchRoot>,
}

impl ReplayOutcome {
    fn new(
        final_state: EthereumState,
        applied_range: Option<AppliedRange>,
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
    pub fn applied_range(&self) -> Option<&AppliedRange> {
        self.applied_range.as_ref()
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
) -> Result<ReplayOutcome, ReplayError>
where
    A: IntoIterator<Item = (Address, GenesisAccount)>,
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let state = ethereum_state_from_genesis_accounts(genesis_accounts)
        .map_err(|source| ReplayError::GenesisState { source })?;
    replay_from_state(
        state,
        GENESIS_FIRST_UPDATE_SEQ_NO,
        Some(GENESIS_LAST_APPLIED_BLOCK_NUM),
        batches,
    )
}

/// Replays ordered batches starting from a validated state snapshot.
pub fn replay_from_snapshot<I>(
    snapshot: ReplayStateSnapshot,
    batches: I,
) -> Result<ReplayOutcome, ReplayError>
where
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let (next_update_seq_no, last_applied_block_num, state) = snapshot.into_parts();
    replay_from_state(state, next_update_seq_no, last_applied_block_num, batches)
}

fn replay_from_state<I>(
    mut state: EthereumState,
    anchor_update_seq_no: u64,
    anchor_block_num: Option<u64>,
    batches: I,
) -> Result<ReplayOutcome, ReplayError>
where
    I: IntoIterator<Item = EvmReplayBatch>,
{
    let mut next_update_seq_no = anchor_update_seq_no;
    let mut previous_block_num = anchor_block_num;
    let mut first_block_num = None;
    let mut applied_roots = Vec::new();

    for batch in batches {
        let update_seq_no = batch.update_seq_no();
        let evm_header = *batch.evm_header();

        if update_seq_no != next_update_seq_no {
            return Err(ReplayError::UnexpectedSeqNo {
                expected: next_update_seq_no,
                actual: update_seq_no,
            });
        }
        if let Some(previous) = previous_block_num {
            if evm_header.block_num <= previous {
                return Err(ReplayError::BlockContinuityViolation {
                    update_seq_no,
                    expected_after_block_num: previous,
                    actual_block_num: evm_header.block_num,
                });
            }
        }
        let following_update_seq_no = update_seq_no
            .checked_add(1)
            .ok_or(ReplayError::TerminalUpdateSeqNo { update_seq_no })?;

        apply_batch_state_diff_to_ethereum_state(&mut state, batch.state_diff())?;
        let post_state_root = state.state_root_buf32();
        if applied_roots.is_empty() {
            first_block_num = previous_block_num.and_then(|block_num| block_num.checked_add(1));
        }
        previous_block_num = Some(evm_header.block_num);
        next_update_seq_no = following_update_seq_no;
        applied_roots.push(AppliedBatchRoot::new(
            update_seq_no,
            evm_header,
            post_state_root,
        ));
    }

    let applied_range = AppliedRange::from_applied_roots(first_block_num, &applied_roots);
    Ok(ReplayOutcome::new(state, applied_range, applied_roots))
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

    fn make_genesis_account(balance: u64, nonce: u64) -> GenesisAccount {
        GenesisAccount {
            nonce: Some(nonce),
            balance: U256::from(balance),
            code: None,
            storage: Some(BTreeMap::new()),
            private_key: None,
        }
    }

    fn make_genesis_account_with_storage(slot_key: U256, slot_value: U256) -> GenesisAccount {
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

    fn make_evm_header(block_num: u64) -> EvmHeaderSummary {
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

    fn make_batch(update_seq_no: u64, block_num: u64, seed: u8) -> EvmReplayBatch {
        EvmReplayBatch::new(
            update_seq_no,
            make_evm_header(block_num),
            make_account_creation_diff(seed),
        )
    }

    #[test]
    fn test_ordered_batches_produce_final_root() {
        let batches = vec![
            make_batch(0, 3, 0x10),
            make_batch(1, 6, 0x11),
            make_batch(2, 9, 0x12),
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
        let applied_range = result.applied_range().expect("range is present");
        assert_eq!(applied_range.first_update_seq_no(), 0);
        assert_eq!(applied_range.last_update_seq_no(), 2);
        assert_eq!(applied_range.first_block_num(), Some(1));
        assert_eq!(applied_range.last_block_num(), 9);
        assert_eq!(result.applied_roots().len(), 3);
    }

    #[test]
    fn test_non_empty_diff_updates_account_and_storage() {
        let address = addr(0x11);
        let batch = EvmReplayBatch::new(
            0,
            make_evm_header(3),
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
    fn test_empty_input_preserves_start_state() {
        let result = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), Vec::new())
            .expect("empty replay succeeds");

        assert!(result.applied_roots().is_empty());
        assert_eq!(result.applied_range(), None);
        assert_eq!(
            result.final_state_root(),
            make_empty_ethereum_state().state_root_buf32()
        );
    }

    #[test]
    fn test_invalid_state_diff_returns_error() {
        let address = addr(0x11);
        let mut state = ethereum_state_from_genesis_accounts([(
            address,
            make_genesis_account_with_storage(slot(1), value(10)),
        )])
        .expect("genesis state builds");
        // Force an incomplete sparse witness so the state-diff applier rejects it.
        state.storage_tries.clear();
        let snapshot = ReplayStateSnapshot::new(8, Some(10), state);
        let batch = EvmReplayBatch::new(8, make_evm_header(11), make_storage_update_diff(0x11));

        let err = replay_from_snapshot(snapshot, [batch]).expect_err("state diff rejects");

        assert!(matches!(err, ReplayError::ApplyDiff(_)));
    }

    #[test]
    fn test_snapshot_anchor_sets_applied_range() {
        let snapshot_state =
            ethereum_state_from_genesis_accounts([(addr(0x01), make_genesis_account(100, 1))])
                .expect("snapshot state builds");
        let snapshot = ReplayStateSnapshot::new(5, Some(10), snapshot_state);
        let batches = vec![make_batch(5, 11, 0x20), make_batch(6, 14, 0x21)];

        let result = replay_from_snapshot(snapshot, batches).expect("snapshot replay succeeds");

        assert_eq!(result.applied_roots().len(), 2);
        let applied_range = result.applied_range().expect("range is present");
        assert_eq!(applied_range.first_update_seq_no(), 5);
        assert_eq!(applied_range.last_update_seq_no(), 6);
        assert_eq!(applied_range.first_block_num(), Some(11));
        assert_eq!(applied_range.last_block_num(), 14);
        assert_eq!(result.applied_roots().len(), 2);
    }

    #[test]
    fn test_update_seqno_gap_returns_error() {
        let batches = vec![make_batch(0, 3, 0x10), make_batch(2, 6, 0x11)];

        let err = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), batches)
            .expect_err("gap rejects");

        assert!(matches!(
            err,
            ReplayError::UnexpectedSeqNo {
                expected: 1,
                actual: 2,
            }
        ));
    }

    #[test]
    fn test_non_increasing_block_number_returns_error() {
        let batches = vec![make_batch(0, 3, 0x10), make_batch(1, 3, 0x11)];

        let err = replay_from_genesis(Vec::<(Address, GenesisAccount)>::new(), batches)
            .expect_err("non-increasing block rejects");

        assert!(matches!(
            err,
            ReplayError::BlockContinuityViolation {
                update_seq_no: 1,
                expected_after_block_num: 3,
                actual_block_num: 3,
            }
        ));
    }

    #[test]
    fn test_terminal_update_seqno_returns_error() {
        let batches = vec![make_batch(u64::MAX, 3, 0x10)];
        let snapshot = ReplayStateSnapshot::new(u64::MAX, Some(2), make_empty_ethereum_state());

        let err = replay_from_snapshot(snapshot, batches).expect_err("terminal seqno rejects");

        assert!(matches!(
            err,
            ReplayError::TerminalUpdateSeqNo {
                update_seq_no: u64::MAX,
            }
        ));
    }
}
