//! State for the batch builder task.

use std::collections::VecDeque;

use alpen_ee_common::{Batch, BatchStorage, BlockNumHash};
use eyre::Result;
use strata_predicate::PredicateKey;

use crate::sealing_policy::{AccumulationPolicy, Accumulator};

/// State for the batch builder task.
///
/// This tracks the current position in the chain and the accumulator for
/// the pending batch. This state is not persisted; it is rebuilt from
/// [`BatchStorage`] on restart.
#[derive(Debug)]
pub struct BatchBuilderState<P: AccumulationPolicy> {
    /// Hash of the last block in the most recent sealed batch (or genesis if no batches).
    prev_batch_end: BlockNumHash,
    /// Index for the next batch to be sealed.
    /// ie. the batch we are currently accumulating blocks for.
    next_batch_idx: u64,
    /// Accumulator for the pending batch.
    accumulator: Accumulator<P>,
    /// Queue of block hashes waiting to be processed (data may not be ready yet).
    pending_blocks: VecDeque<BlockNumHash>,
    /// The update VK the batch currently being accumulated is proven under.
    current_update_vk: PredicateKey,
    /// Set when the last accumulated block consumed a VK-update message: the
    /// batch must be sealed at that block, before any further block is added.
    seal_pending: bool,
    /// The key the account rotates to at the pending seal, if any.
    pending_update_vk: Option<PredicateKey>,
}

impl<P: AccumulationPolicy> BatchBuilderState<P> {
    /// Initialize state from the last sealed batch.
    ///
    /// Used when resuming from storage where batches already exist.
    pub fn from_last_batch(batch: &Batch) -> Self {
        Self {
            prev_batch_end: batch.last_blocknumhash(),
            next_batch_idx: batch.idx() + 1,
            accumulator: Accumulator::new(),
            pending_blocks: VecDeque::new(),
            current_update_vk: batch.next_update_vk().clone(),
            seal_pending: false,
            pending_update_vk: None,
        }
    }

    /// Get the hash of the last block in the previous batch.
    pub fn prev_batch_end(&self) -> BlockNumHash {
        self.prev_batch_end
    }

    /// Get the index for the next batch to be sealed.
    /// This is the batch we are currently accumulating blocks for.
    pub fn next_batch_idx(&self) -> u64 {
        self.next_batch_idx
    }

    /// Get a reference to the accumulator.
    pub fn accumulator(&self) -> &Accumulator<P> {
        &self.accumulator
    }

    /// Get a mutable reference to the accumulator.
    pub fn accumulator_mut(&mut self) -> &mut Accumulator<P> {
        &mut self.accumulator
    }

    /// Called after sealing a batch.
    ///
    /// Advances the state to prepare for the next batch, rotating the
    /// current update VK when the sealed batch consumed a VK-update message.
    pub fn advance_batch(&mut self, new_prev_batch_end: BlockNumHash) {
        self.prev_batch_end = new_prev_batch_end;
        self.next_batch_idx += 1;
        self.accumulator.reset();
        self.seal_pending = false;
        if let Some(new_vk) = self.pending_update_vk.take() {
            self.current_update_vk = new_vk;
        }
    }

    /// The update VK the batch currently being accumulated is proven under.
    pub fn current_update_vk(&self) -> &PredicateKey {
        &self.current_update_vk
    }

    /// The key the account rotates to at the pending seal, if any.
    pub fn pending_update_vk(&self) -> Option<&PredicateKey> {
        self.pending_update_vk.as_ref()
    }

    /// Whether the next processed block must first seal the current batch.
    pub fn is_seal_pending(&self) -> bool {
        self.seal_pending
    }

    /// Marks the just-accumulated block as a VK-update boundary: the batch
    /// seals at it, and the account rotates to `new_vk` afterwards.
    pub fn mark_vk_update_boundary(&mut self, new_vk: PredicateKey) {
        self.seal_pending = true;
        self.pending_update_vk = Some(new_vk);
    }

    /// Clears boundary bookkeeping. Called on reorg resets alongside
    /// accumulator/pending-queue clears.
    pub fn reset_boundary_marks(&mut self) {
        self.seal_pending = false;
        self.pending_update_vk = None;
    }

    /// Returns the first pending block hash, if any.
    pub fn first_pending_block(&self) -> Option<BlockNumHash> {
        self.pending_blocks.front().copied()
    }

    /// Returns true if there are pending blocks to process.
    pub fn has_pending_blocks(&self) -> bool {
        !self.pending_blocks.is_empty()
    }

    /// Removes and returns the first pending block.
    pub fn pop_pending_block(&mut self) -> Option<BlockNumHash> {
        self.pending_blocks.pop_front()
    }

    /// Adds blocks to the pending queue.
    pub fn push_pending_blocks(&mut self, blocks: impl IntoIterator<Item = BlockNumHash>) {
        self.pending_blocks.extend(blocks);
    }

    /// Clears the pending blocks queue.
    pub fn clear_pending_blocks(&mut self) {
        self.pending_blocks.clear();
    }

    /// Returns the last block in the pending queue, or the last accumulated block,
    /// or the previous batch end. Used to determine the starting point for fetching
    /// new blocks.
    pub fn last_known_block(&self) -> BlockNumHash {
        self.pending_blocks
            .back()
            .copied()
            .or_else(|| self.accumulator.last_block())
            .unwrap_or(self.prev_batch_end)
    }
}

/// Initialize batch builder state from storage.
///
/// If batches exist in storage, resumes from the last batch.
/// Otherwise, starts fresh from genesis.
pub async fn init_batch_builder_state<P: AccumulationPolicy>(
    batch_storage: &impl BatchStorage,
) -> Result<BatchBuilderState<P>> {
    let (batch, _) = batch_storage
        .get_latest_batch()
        .await?
        .ok_or_else(|| eyre::eyre!("no batches in storage; genesis batch expected"))?;
    Ok(BatchBuilderState::from_last_batch(&batch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sealing_policy::block_count_policy::BlockCountPolicy, test_utils::*};

    fn test_batch(idx: u64, last_block: BlockNumHash) -> Batch {
        Batch::new(
            idx,
            test_blocknumhash(200 + idx as u8).hash(),
            last_block.hash(),
            last_block.blocknum(),
            vec![],
            PredicateKey::always_accept(),
            PredicateKey::always_accept(),
        )
        .unwrap()
    }

    #[test]
    fn test_from_last_batch() {
        let last_block = test_blocknumhash(10);
        let state: BatchBuilderState<BlockCountPolicy> =
            BatchBuilderState::from_last_batch(&test_batch(5, last_block));

        assert_eq!(state.prev_batch_end(), last_block);
        assert_eq!(state.next_batch_idx(), 6);
        assert!(state.accumulator().is_empty());
    }

    #[test]
    fn test_advance_batch() {
        let genesis = test_blocknumhash(1);
        let genesis_batch = Batch::new_genesis_batch(
            genesis.hash(),
            genesis.blocknum(),
            PredicateKey::always_accept(),
        )
        .unwrap();
        let mut state: BatchBuilderState<BlockCountPolicy> =
            BatchBuilderState::from_last_batch(&genesis_batch);

        // After from_last_batch(0, ...), next_batch_idx is 1
        assert_eq!(state.next_batch_idx(), 1);

        let new_end = test_blocknumhash(5);
        state.advance_batch(new_end);

        // After advance_batch, next_batch_idx is incremented to 2
        assert_eq!(state.prev_batch_end(), new_end);
        assert_eq!(state.next_batch_idx(), 2);
        assert!(state.accumulator().is_empty());
    }
}
