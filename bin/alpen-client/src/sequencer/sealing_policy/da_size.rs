//! DA-size based batch sealing.
//!
//! Seals a batch once the estimated encoded size of its state diff would exceed a
//! configured byte limit. The pending batch is accumulated in a [`BatchBuilder`] and
//! sized through [`estimate_da_size`]; the incoming block is sized on a [`ProjectedBatch`]
//! view so shared and reverted entries are counted exactly once.

use std::sync::Arc;

use alpen_ee_sequencer::sealing_policy::{
    AccumulationPolicy, BlockDataProvider, SealReason, SealingPolicy,
};
use alpen_reth_db::StateDiffProvider;
use alpen_reth_statediff::{estimate_da_size, BatchBuilder, BlockStateChanges};
use async_trait::async_trait;
use strata_acct_types::Hash;

use super::projected_batch::ProjectedBatch;

/// Fetches per-block state changes from the state diff store.
#[derive(Debug, Clone)]
pub(crate) struct DaSizeProvider<D> {
    state_diff_provider: Arc<D>,
}

impl<D> DaSizeProvider<D> {
    pub(crate) fn new(state_diff_provider: Arc<D>) -> Self {
        Self {
            state_diff_provider,
        }
    }
}

#[async_trait]
impl<D: StateDiffProvider> BlockDataProvider<DaBatchSizePolicy> for DaSizeProvider<D> {
    async fn get_block_data(&self, hash: Hash) -> eyre::Result<Option<BlockStateChanges>> {
        Ok(self
            .state_diff_provider
            .get_state_diff_by_hash(hash.0.into())?)
    }
}

/// Accumulates per-block state changes into a [`BatchBuilder`] for the pending batch.
#[derive(Debug)]
pub(crate) struct DaBatchSizePolicy;

impl AccumulationPolicy for DaBatchSizePolicy {
    type BlockData = BlockStateChanges;
    type AccumulatedValue = BatchBuilder;

    fn accumulate(
        accumulated_batch_diff: &mut Self::AccumulatedValue,
        block_diff: &Self::BlockData,
    ) {
        accumulated_batch_diff.apply_block(block_diff);
    }
}

/// Seals a batch when its estimated DA size would exceed the configured limit.
#[derive(Debug)]
pub(crate) struct MaxDaSizeSealing {
    max_size: u64,
}

impl MaxDaSizeSealing {
    pub(crate) fn new(max_size: u64) -> Self {
        Self { max_size }
    }

    pub(crate) fn max_size(&self) -> u64 {
        self.max_size
    }
}

impl SealingPolicy<DaBatchSizePolicy> for MaxDaSizeSealing {
    fn name(&self) -> SealReason {
        "da_size"
    }

    fn would_exceed(
        &self,
        accumulated_diff: &BatchBuilder,
        block_diff: &BlockStateChanges,
    ) -> bool {
        estimate_da_size(&ProjectedBatch::new(accumulated_diff, block_diff)) > self.max_size()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, KECCAK256_EMPTY, U256};
    use alpen_reth_statediff::{AccountSnapshot, BlockAccountChange, BlockStateChanges};

    use super::*;

    fn snapshot(balance: u64, nonce: u64) -> AccountSnapshot {
        AccountSnapshot {
            balance: U256::from(balance),
            nonce,
            code_hash: KECCAK256_EMPTY,
        }
    }

    fn block_touching(addr: Address, from: u64, to: u64) -> BlockStateChanges {
        let mut block = BlockStateChanges::new();
        block.accounts.insert(
            addr,
            BlockAccountChange {
                original: Some(snapshot(from, 0)),
                current: Some(snapshot(to, 1)),
            },
        );
        block
    }

    #[test]
    fn would_exceed_compares_projected_size_to_limit() {
        let block = block_touching(Address::from([0x11u8; 20]), 0, 1);
        let empty = BatchBuilder::new();
        let total = estimate_da_size(&ProjectedBatch::new(&empty, &block));

        // A limit exactly at the projected size admits the block; one byte less seals.
        assert!(!MaxDaSizeSealing::new(total).would_exceed(&empty, &block));
        assert!(MaxDaSizeSealing::new(total - 1).would_exceed(&empty, &block));
    }

    #[test]
    fn would_exceed_does_not_recount_accounts_already_in_the_batch() {
        let addr = Address::from([0x11u8; 20]);
        let mut pending = BatchBuilder::new();
        pending.apply_block(&block_touching(addr, 0, 1));
        let next = block_touching(addr, 1, 2);

        // Touching the same account again adds no bytes, so the batch size is the limit.
        let size = estimate_da_size(&pending);
        assert!(!MaxDaSizeSealing::new(size).would_exceed(&pending, &next));
        assert!(MaxDaSizeSealing::new(size - 1).would_exceed(&pending, &next));
    }
}
