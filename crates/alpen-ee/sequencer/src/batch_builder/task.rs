//! Batch builder task implementation.

use std::time::Duration;

use alpen_ee_common::{Batch, BatchId, BatchStorage, BlockNumHash, ExecBlockStorage};
use eyre::{eyre, Result};
use strata_acct_types::Hash;
use tokio::{sync::mpsc, time};
use tracing::{debug, error, warn};

use super::{ctx::BatchBuilderCtx, events::BatchBuilderEvent, BatchBuilderState};
use crate::{
    batch_builder::reorg::{check_and_handle_reorg, ReorgReport},
    sealing_policy::{AccumulationPolicy, BlockDataProvider, SealingPolicy},
};

/// Polling interval for checking pending block data availability.
const PENDING_BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of blocks to process in a single polling cycle.
/// This prevents blocking the select loop for too long when many blocks have data ready.
const MAX_BLOCKS_PER_CYCLE: usize = 10;

/// Get block hashes and heights from `from_hash` (exclusive) to `to_hash` (inclusive).
///
/// Walks backwards from `to_hash` until reaching `from_hash`.
/// Returns an empty vec if `from_hash == to_hash`.
async fn get_block_range(
    from_hash: Hash,
    to_hash: Hash,
    block_storage: &impl ExecBlockStorage,
) -> Result<Vec<BlockNumHash>> {
    // Ensure endpoint exists
    let from_block = block_storage
        .get_exec_block(from_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("Block not found: from_hash = {}", from_hash))?;

    if from_hash == to_hash {
        return Ok(Vec::new());
    }

    let mut blocks = Vec::new();
    let mut current_hash = to_hash;

    while current_hash != from_hash {
        let current_block = block_storage
            .get_exec_block(current_hash)
            .await?
            .ok_or_else(|| eyre::eyre!("Block not found: {}", current_hash))?;

        if current_block.blocknum() < from_block.blocknum() {
            return Err(eyre!(
                "to_hash ({}) does not extend from_hash ({})",
                to_hash,
                from_hash
            ));
        }

        blocks.push(current_block.blocknumhash());
        current_hash = current_block.parent_blockhash();
    }

    blocks.reverse();
    Ok(blocks)
}

/// Seal the current batch.
///
/// Returns the sealed batch ID, or `None` if accumulator was empty.
///
/// Chunk creation is handled by the downstream chunk builder, which
/// receives a [`BatchBuilderEvent::BatchSealed`] for the sealed batch.
async fn seal_batch<P: AccumulationPolicy>(
    state: &mut BatchBuilderState<P>,
    storage: &impl BatchStorage,
) -> Result<Option<BatchId>> {
    // Read the accumulated blocks without releasing them. `save_next_batch`
    // below can fail, and this state is never persisted: the task only logs
    // the error and keeps polling, so blocks released before the write would
    // never reach any batch. `prev_batch_end` would still sit before them, so
    // the next seal would write a batch covering a range its `inner_blocks`
    // doesn't list.
    let (last_block, inner_blocks) = {
        let Some((last, inner)) = state.accumulator().blocks().split_last() else {
            return Ok(None);
        };
        (*last, inner.iter().map(|b| b.hash()).collect::<Vec<Hash>>())
    };

    let prev_block = state.prev_batch_end();
    let batch_idx = state.next_batch_idx();
    let batch = Batch::new(
        batch_idx,
        prev_block.hash(),
        last_block.hash(),
        last_block.blocknum(),
        inner_blocks,
    )
    .map_err(|err| eyre!(err))?;
    let batch_id = batch.id();

    debug!(
        batch_idx = batch.idx(),
        prev_block = %prev_block.hash(),
        last_block = %last_block.hash(),
        "Sealing batch"
    );

    storage.save_next_batch(batch).await?;

    // The batch is durable, so the blocks it covers can go.
    // `advance_batch` resets the accumulator.
    state.advance_batch(last_block);

    Ok(Some(batch_id))
}

/// Seal the accumulated batch if the sealing policy requires it as it stands.
///
/// Called after admitting a block, and again at the top of each cycle: the
/// requirement is read off the accumulated value, so a write that failed
/// earlier is retried here before anything else is admitted.
async fn seal_if_required<P, D, S, BS, ES>(
    state: &mut BatchBuilderState<P>,
    ctx: &BatchBuilderCtx<P, D, S, BS, ES>,
) -> Result<()>
where
    P: AccumulationPolicy,
    D: BlockDataProvider<P>,
    S: SealingPolicy<P>,
    BS: BatchStorage,
    ES: ExecBlockStorage,
{
    if !state.accumulator().must_seal(&ctx.sealing_policy) {
        return Ok(());
    }

    if let Some(batch_id) = seal_batch(state, ctx.batch_storage.as_ref()).await? {
        let _ = ctx.latest_batch_tx.send(batch_id);
        emit_event(&ctx.event_tx, BatchBuilderEvent::batch_sealed(batch_id)).await;
    }

    Ok(())
}

/// Main batch builder task.
///
/// This task monitors the canonical chain and builds batches according to the
/// sealing policy. It handles reorgs and persists sealed batches to storage.
///
/// The task uses two concurrent branches:
/// 1. React to new canonical tips, check for reorgs, and queue unprocessed blocks
/// 2. Process blocks from the queue when their data becomes available
pub(crate) async fn batch_builder_task<P, D, S, BS, ES>(
    mut state: BatchBuilderState<P>,
    mut ctx: BatchBuilderCtx<P, D, S, BS, ES>,
) where
    P: AccumulationPolicy,
    D: BlockDataProvider<P>,
    S: SealingPolicy<P>,
    BS: BatchStorage,
    ES: ExecBlockStorage,
{
    let mut pending_poll_interval = time::interval(PENDING_BLOCK_POLL_INTERVAL);

    loop {
        let result = tokio::select! {
            // Branch 1: New canonical tip received
            changed = ctx.preconf_rx.changed() => {
                if changed.is_err() {
                    warn!("preconf_rx channel closed; exiting");
                    return;
                }
                let new_tip = *ctx.preconf_rx.borrow_and_update();
                debug!("canonical tip received: {:?}", new_tip );
                handle_new_tip(&mut state, &ctx, new_tip).await
            }

            // Branch 2: Periodically poll pending blocks when queue is non-empty
            // Also fires with an empty queue when a seal is still required, so
            // a write that failed earlier is retried rather than waiting on
            // the next block to arrive.
            _ = pending_poll_interval.tick(), if state.has_pending_blocks()
                || state.accumulator().must_seal(&ctx.sealing_policy) => {
                debug!("processing pending blocks");
                process_pending_blocks(&mut state, &ctx).await
            }
        };

        if let Err(e) = result {
            error!(error = %e, "Batch builder error");
        }
    }
}

/// Handle a new canonical tip update.
///
/// Checks for reorgs and queues any new blocks for processing.
async fn handle_new_tip<P, D, S, BS, ES>(
    state: &mut BatchBuilderState<P>,
    ctx: &BatchBuilderCtx<P, D, S, BS, ES>,
    new_tip: BlockNumHash,
) -> Result<()>
where
    P: AccumulationPolicy,
    D: BlockDataProvider<P>,
    S: SealingPolicy<P>,
    BS: BatchStorage,
    ES: ExecBlockStorage,
{
    // Check and handle reorgs first
    match check_and_handle_reorg(
        state,
        &ctx.canonical_reader(),
        ctx.batch_storage.as_ref(),
        ctx.genesis,
    )
    .await?
    {
        ReorgReport::NoReorg => {
            // No reorg detected.
            // Continue normal execution.
        }
        ReorgReport::ShallowReorg => {
            // Shallow reorg. Pending blocks and accumulator reset.
            // Latest sealed batch has not changed.
            emit_event(
                &ctx.event_tx,
                BatchBuilderEvent::reorg(
                    state.prev_batch_end(),
                    state.next_batch_idx().saturating_sub(1),
                ),
            )
            .await;
        }
        ReorgReport::Reorg(batch_id) => {
            // Unfinalized batch has been reorg'd.
            // Latest batch reverted. Pending blocks and accumulator reset.
            // State is already reset by check_and_handle_reorg;
            let _ = ctx.latest_batch_tx.send(batch_id);
            emit_event(
                &ctx.event_tx,
                BatchBuilderEvent::reorg(
                    state.prev_batch_end(),
                    state.next_batch_idx().saturating_sub(1),
                ),
            )
            .await;
        }
        ReorgReport::DeepReorg => {
            // TODO(STR-3682): unrecoverable error
            return Err(eyre!("deep reorg detected"));
        }
    }

    // Determine starting point for fetching new blocks
    let last_known = state.last_known_block();

    // Get blocks from start to new tip and add to pending queue
    let blocks = get_block_range(
        last_known.hash(),
        new_tip.hash(),
        ctx.block_storage.as_ref(),
    )
    .await?;

    if !blocks.is_empty() {
        debug!(
            count = blocks.len(),
            start = %last_known.hash(),
            tip = %new_tip.hash(),
            "Queuing new blocks"
        );
        state.push_pending_blocks(blocks);
    }

    Ok(())
}

/// Process pending blocks whose data is ready.
///
/// Processes blocks sequentially from the front of the queue. Stops when
/// a block's data is not yet available, the queue is empty, or the maximum
/// number of blocks per cycle is reached.
async fn process_pending_blocks<P, D, S, BS, ES>(
    state: &mut BatchBuilderState<P>,
    ctx: &BatchBuilderCtx<P, D, S, BS, ES>,
) -> Result<()>
where
    P: AccumulationPolicy,
    D: BlockDataProvider<P>,
    S: SealingPolicy<P>,
    BS: BatchStorage,
    ES: ExecBlockStorage,
{
    let mut processed = 0;

    // Process blocks while data is available, up to the max per cycle
    while processed < MAX_BLOCKS_PER_CYCLE {
        // Settle a seal the accumulator still requires. Normally this fires
        // below, right after the block that required it; running it here too
        // retries a write that failed on an earlier cycle, before anything can
        // join a batch that should already be closed.
        seal_if_required(state, ctx).await?;

        let Some(block) = state.first_pending_block() else {
            break;
        };

        // Try to get block data (non-blocking check)
        let Some(block_data) = ctx.block_data_provider.get_block_data(block.hash()).await? else {
            // Data not ready yet, stop processing
            debug!(hash = %block.hash(), "block data not yet ready");
            break;
        };

        // Check if adding this block would exceed threshold. The write happens
        // before the block leaves the queue, so a failed seal leaves it at the
        // front for the next poll to retry.
        if !state.accumulator().is_empty()
            && state
                .accumulator()
                .would_exceed(&ctx.sealing_policy, &block_data)
        {
            if let Some(batch_id) = seal_batch(state, ctx.batch_storage.as_ref()).await? {
                // Notify watchers of new batch
                let _ = ctx.latest_batch_tx.send(batch_id);
                emit_event(&ctx.event_tx, BatchBuilderEvent::batch_sealed(batch_id)).await;
            }
        }

        // Data is ready, remove from pending queue and accumulate it
        state.pop_pending_block();
        state.accumulator_mut().add_block(block, &block_data);
        emit_event(
            &ctx.event_tx,
            BatchBuilderEvent::block_admitted(block, state.next_batch_idx()),
        )
        .await;

        // The policy may require this block to end its batch — a predicate
        // rotation does, since anything after it would stay in an update
        // proven against the key the rotation retires. See
        // `handle_batch_boundary` in the chunk builder for the mirrored
        // chunk-level seal this triggers downstream.
        seal_if_required(state, ctx).await?;

        debug!(hash = %block.hash(), "Processed block");
        processed += 1;
    }

    Ok(())
}

/// Send a [`BatchBuilderEvent`] if the channel is configured. Logs a warning
/// if the receiver has been dropped (should not happen in production).
async fn emit_event(tx: &Option<mpsc::Sender<BatchBuilderEvent>>, event: BatchBuilderEvent) {
    if let Some(tx) = tx {
        if let Err(e) = tx.send(event).await {
            warn!(error = %e, "batch event channel closed; event dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        marker::PhantomData,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use alpen_ee_common::{
        Batch, BatchId, ExecBlockRecord, MockBatchStorage, MockExecBlockStorage, StorageError,
    };
    use alpen_ee_exec_chain::{ExecChainHandle, ExecChainMsg};
    use alpen_ee_params::AlpenSpecId;
    use strata_ee_acct_types::EeAccountState;
    use strata_ee_chain_types::{ExecBlockCommitment, ExecBlockPackage, ExecInputs, ExecOutputs};
    use strata_identifiers::{Buf32, OLBlockCommitment};
    use strata_predicate::PredicateKey;
    use strata_service::CommandHandle;
    use tokio::sync::watch;

    use super::*;
    use crate::{
        batch_builder::BatchBuilderState,
        sealing_policy::{
            block_count_policy::{
                BlockCountData, BlockCountDataProvider, BlockCountPolicy, FixedBlockCountSealing,
            },
            or_policy::{ComposedDataProvider, ComposedPolicy, OrSealing},
            rotation_policy::{RotationDataProvider, RotationPolicy, SealOnRotation},
        },
        test_utils::*,
    };

    /// The batch builder's real policy shape: block count for cadence, rotation
    /// for the protocol boundary.
    type TestPolicy = ComposedPolicy<BlockCountPolicy, RotationPolicy>;
    type TestSealing =
        OrSealing<BlockCountPolicy, RotationPolicy, FixedBlockCountSealing, SealOnRotation>;
    type TestProvider = ComposedDataProvider<
        BlockCountPolicy,
        RotationPolicy,
        BlockCountDataProvider,
        RotationDataProvider<MockExecBlockStorage>,
    >;

    /// Builds an exec block record, optionally declaring a consumed predicate
    /// rotation in its package outputs.
    fn exec_record(block: BlockNumHash, rotation: Option<PredicateKey>) -> ExecBlockRecord {
        let mut outputs = ExecOutputs::new_empty();
        outputs.set_new_predicate(rotation);

        let package = ExecBlockPackage::new(
            ExecBlockCommitment::new(block.hash(), block.hash()),
            ExecInputs::new_empty(),
            outputs,
        );

        ExecBlockRecord::new(
            package,
            EeAccountState::new(block.hash(), Hash::zero(), vec![], vec![]),
            block.blocknum(),
            OLBlockCommitment::new(block.blocknum(), Buf32::new([0u8; 32]).into()),
            1_000_000,
            Hash::default(),
            0,
            0,
            AlpenSpecId::V0,
            vec![],
        )
    }

    /// Assembles a context whose block-count cadence never fires on its own, so
    /// the only thing that can seal a batch here is the rotation rule.
    ///
    /// `preconf_rx` and `exec_chain` are unused by `process_pending_blocks`, so
    /// their counterparts are dropped straight away.
    fn test_ctx(
        block_storage: MockExecBlockStorage,
        batch_storage: MockBatchStorage,
        genesis: BlockNumHash,
    ) -> BatchBuilderCtx<
        TestPolicy,
        TestProvider,
        TestSealing,
        MockBatchStorage,
        MockExecBlockStorage,
    > {
        test_ctx_with_cap(block_storage, batch_storage, genesis, 1000)
    }

    /// As [`test_ctx`], with an explicit block-count cap so a test can drive
    /// the ordinary threshold seal.
    fn test_ctx_with_cap(
        block_storage: MockExecBlockStorage,
        batch_storage: MockBatchStorage,
        genesis: BlockNumHash,
        cap: u64,
    ) -> BatchBuilderCtx<
        TestPolicy,
        TestProvider,
        TestSealing,
        MockBatchStorage,
        MockExecBlockStorage,
    > {
        let (_, preconf_rx) = watch::channel(genesis);
        let (cmd_tx, _) = mpsc::channel::<ExecChainMsg>(1);
        let (latest_batch_tx, _) =
            watch::channel(BatchId::from_parts(genesis.hash(), genesis.hash()));

        let block_storage = Arc::new(block_storage);

        BatchBuilderCtx {
            genesis,
            preconf_rx,
            block_data_provider: Arc::new(ComposedDataProvider::new(
                BlockCountDataProvider,
                RotationDataProvider::new(block_storage.clone()),
            )),
            sealing_policy: OrSealing::new(FixedBlockCountSealing::new(cap), SealOnRotation),
            block_storage,
            batch_storage: Arc::new(batch_storage),
            exec_chain: ExecChainHandle::new(CommandHandle::new(cmd_tx)),
            latest_batch_tx,
            event_tx: None,
            _policy: PhantomData,
        }
    }

    /// A failed batch write must leave the accumulated blocks alone. Releasing
    /// them before the write loses them for good: this state isn't persisted,
    /// the task only logs the error and keeps polling, and `prev_batch_end`
    /// still sits before them — so the next seal would write a batch covering
    /// a range its `inner_blocks` doesn't list.
    #[tokio::test]
    async fn failed_batch_write_keeps_the_accumulated_blocks() {
        let genesis = test_blocknumhash(0);
        let block1 = test_blocknumhash(1);
        let block2 = test_blocknumhash(2);

        let mut state: BatchBuilderState<BlockCountPolicy> =
            BatchBuilderState::from_last_batch(0, genesis);
        state.accumulator_mut().add_block(block1, &BlockCountData);
        state.accumulator_mut().add_block(block2, &BlockCountData);

        let mut batch_storage = MockBatchStorage::new();
        batch_storage
            .expect_save_next_batch()
            .returning(|_| Err(StorageError::Database("transient".to_string())));

        let result = seal_batch(&mut state, &batch_storage).await;

        assert!(result.is_err(), "a failed batch write must surface");
        assert_eq!(
            state.accumulator().blocks(),
            [block1, block2],
            "the blocks must stay accumulated so the next seal still covers them"
        );
        assert_eq!(state.prev_batch_end(), genesis, "no batch was sealed");
        assert_eq!(
            state.next_batch_idx(),
            1,
            "the batch index must not advance"
        );
    }

    /// The rule this exists for: a rotation-consuming block ends its batch,
    /// even though the block-count cadence is nowhere near its threshold.
    #[tokio::test]
    async fn rotation_block_seals_its_batch() {
        let genesis = test_blocknumhash(0);
        let block1 = test_blocknumhash(1);

        let mut state: BatchBuilderState<TestPolicy> =
            BatchBuilderState::from_last_batch(0, genesis);
        state.push_pending_blocks(vec![block1]);

        let mut block_storage = MockExecBlockStorage::new();
        block_storage.expect_get_exec_block().returning(move |_| {
            Ok(Some(exec_record(
                block1,
                Some(PredicateKey::always_accept()),
            )))
        });

        let mut batch_storage = MockBatchStorage::new();
        batch_storage.expect_save_next_batch().returning(|_| Ok(()));

        let ctx = test_ctx(block_storage, batch_storage, genesis);

        process_pending_blocks(&mut state, &ctx)
            .await
            .expect("processing should succeed");

        assert!(
            state.accumulator().is_empty(),
            "the rotation block must have sealed its batch, draining the accumulator"
        );
        assert_eq!(
            state.prev_batch_end(),
            block1,
            "the sealed batch must end on the rotation block"
        );
    }

    /// A required seal whose write fails must be retried before the next block
    /// is admitted. The requirement lives on the accumulated value, so it
    /// outlives the cycle that incurred it.
    #[tokio::test]
    async fn failed_required_seal_is_retried_before_the_next_block() {
        let genesis = test_blocknumhash(0);
        let block1 = test_blocknumhash(1);
        let block2 = test_blocknumhash(2);

        let mut state: BatchBuilderState<TestPolicy> =
            BatchBuilderState::from_last_batch(0, genesis);
        state.push_pending_blocks(vec![block1, block2]);

        let mut block_storage = MockExecBlockStorage::new();
        block_storage
            .expect_get_exec_block()
            .returning(move |hash| {
                Ok(Some(if hash == block1.hash() {
                    exec_record(block1, Some(PredicateKey::always_accept()))
                } else {
                    exec_record(block2, None)
                }))
            });

        // The first write fails, every later one succeeds.
        let writes = Arc::new(AtomicUsize::new(0));
        let mut batch_storage = MockBatchStorage::new();
        batch_storage.expect_save_next_batch().returning(move |_| {
            if writes.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(StorageError::Database("transient".to_string()))
            } else {
                Ok(())
            }
        });

        let ctx = test_ctx(block_storage, batch_storage, genesis);

        let result = process_pending_blocks(&mut state, &ctx).await;

        assert!(result.is_err(), "the failed seal must surface");
        assert_eq!(state.prev_batch_end(), genesis, "no batch was sealed");
        assert!(
            state.accumulator().must_seal(&ctx.sealing_policy),
            "the requirement must outlive the cycle that failed to meet it"
        );

        process_pending_blocks(&mut state, &ctx)
            .await
            .expect("the retry should succeed");

        assert_eq!(
            state.prev_batch_end(),
            block1,
            "the retried batch must still end on the rotation block"
        );
        assert_eq!(
            state.accumulator().blocks(),
            [block2],
            "the following block must start the next batch, not join the rotation's"
        );
    }

    /// A batch written after an earlier seal failed must still cover every
    /// block in its range. Releasing the accumulator before the write used to
    /// lose the blocks already in it while leaving `prev_batch_end` behind
    /// them, so the next seal wrote a batch whose `prev_block` preceded blocks
    /// its `inner_blocks` never listed. `build_update_from_batch` walks
    /// exactly that list, so those blocks — and any rotation among them —
    /// would never reach the update.
    #[tokio::test]
    async fn batch_written_after_a_failed_seal_covers_every_block() {
        let genesis = test_blocknumhash(0);
        let block1 = test_blocknumhash(1);
        let block2 = test_blocknumhash(2);
        let block3 = test_blocknumhash(3);

        let mut state: BatchBuilderState<TestPolicy> =
            BatchBuilderState::from_last_batch(0, genesis);
        state.push_pending_blocks(vec![block1, block2, block3]);

        let mut block_storage = MockExecBlockStorage::new();
        block_storage
            .expect_get_exec_block()
            .returning(move |hash| {
                let block = [block1, block2, block3]
                    .into_iter()
                    .find(|b| b.hash() == hash)
                    .unwrap_or(genesis);
                Ok(Some(exec_record(block, None)))
            });

        // The first write fails; capture whatever is written after that.
        let writes = Arc::new(AtomicUsize::new(0));
        let written: Arc<Mutex<Vec<Batch>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = written.clone();
        let mut batch_storage = MockBatchStorage::new();
        batch_storage
            .expect_save_next_batch()
            .returning(move |batch| {
                if writes.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(StorageError::Database("transient".to_string()));
                }
                captured.lock().expect("lock").push(batch);
                Ok(())
            });

        // Cap of 2 makes admitting the third block seal the first two.
        let ctx = test_ctx_with_cap(block_storage, batch_storage, genesis, 2);

        let result = process_pending_blocks(&mut state, &ctx).await;
        assert!(result.is_err(), "the failed threshold seal must surface");

        process_pending_blocks(&mut state, &ctx)
            .await
            .expect("the retry should succeed");

        let batches = written.lock().expect("lock");
        assert_eq!(
            batches.len(),
            1,
            "exactly one batch should have been written"
        );
        let batch = &batches[0];

        assert_eq!(
            batch.prev_block(),
            genesis.hash(),
            "batch starts after genesis"
        );
        assert_eq!(
            batch.blocks_iter().collect::<Vec<_>>(),
            vec![block1.hash(), block2.hash()],
            "the batch must list every block between its prev_block and last_block"
        );
    }
}
