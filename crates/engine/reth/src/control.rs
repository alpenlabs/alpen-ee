use std::{fmt::Debug, future::Future, time::Duration};

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use alpen_common::{BlockNumHash, ConsensusHeads};
use alpen_engine_api::ExecutionEngine;
use reth_node_builder::NodeTypesWithDB;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    BlockHashReader, BlockNumReader, CanonStateSubscriptions, ProviderError,
};
use tokio::{
    select,
    sync::{broadcast, watch},
    time,
};
use tracing::{debug, error, warn};

/// How often the engine control task checks for safe and finalized blocks reth is missing.
///
/// Reth's canonical state notifications miss some chain moves, so this is the fallback.
const RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Read access to the parts of the execution engine's chain state the engine control task uses.
trait CanonicalChain {
    /// Error returned when the chain state cannot be read.
    type Error: Debug;

    /// Returns the hash of the canonical head.
    fn head_hash(&self) -> B256;

    /// Returns the hash of the safe block the engine has applied, if any.
    fn safe_hash(&self) -> Option<B256>;

    /// Returns the hash of the finalized block the engine has applied, if any.
    fn finalized_hash(&self) -> Option<B256>;

    /// Returns whether `blockhash` is on the canonical chain.
    fn contains(&self, blockhash: B256) -> Result<bool, Self::Error>;
}

impl<N: NodeTypesWithDB + ProviderNodeTypes> CanonicalChain for BlockchainProvider<N> {
    type Error = ProviderError;

    fn head_hash(&self) -> B256 {
        self.canonical_in_memory_state().get_canonical_head().hash()
    }

    fn safe_hash(&self) -> Option<B256> {
        self.canonical_in_memory_state()
            .get_safe_num_hash()
            .map(|num_hash| num_hash.hash)
    }

    fn finalized_hash(&self) -> Option<B256> {
        self.canonical_in_memory_state()
            .get_finalized_num_hash()
            .map(|num_hash| num_hash.hash)
    }

    fn contains(&self, blockhash: B256) -> Result<bool, ProviderError> {
        let Some(block_number) = self.block_number(blockhash)? else {
            return Ok(false);
        };
        let Some(canonical_blockhash) = self.block_hash(block_number)? else {
            return Ok(false);
        };
        Ok(blockhash == canonical_blockhash)
    }
}

fn forkchoice_state_from_consensus<C: CanonicalChain>(
    consensus_state: &ConsensusHeads,
    head_block_hash: B256,
    chain: &C,
) -> Result<ForkchoiceState, C::Error> {
    let safe_block_hash = B256::from_slice(consensus_state.confirmed().as_slice());
    let finalized_block_hash = B256::from_slice(consensus_state.finalized().as_slice());

    let head_block_hash = if chain.contains(safe_block_hash)? {
        head_block_hash
    } else {
        // Safe block is not in canonical chain on reth.
        // This means either:
        // 1. This is during initial sync and OL chain is ahead of reth
        // 2. There is a fork
        // In either case, OL defines the canonical fork, so prefer OL's state.
        safe_block_hash
    };

    Ok(ForkchoiceState {
        head_block_hash,
        safe_block_hash,
        finalized_block_hash,
    })
}

/// Builds an update that sends reth the OL's safe and finalized blocks it has not applied.
///
/// Reuses `last_sent_head` so reth's sync target does not move, and returns `None` until reth has
/// reached that head. Also returns `None` when there is nothing reth can apply.
fn safe_and_finalized_update<C: CanonicalChain>(
    consensus_state: &ConsensusHeads,
    last_sent_head: B256,
    chain: &C,
) -> Result<Option<ForkchoiceState>, C::Error> {
    if chain.head_hash() != last_sent_head {
        return Ok(None);
    }

    let safe_block_hash = pending_hash(
        B256::from_slice(consensus_state.confirmed().as_slice()),
        chain.safe_hash(),
        chain,
    )?;
    let finalized_block_hash = pending_hash(
        B256::from_slice(consensus_state.finalized().as_slice()),
        chain.finalized_hash(),
        chain,
    )?;

    if safe_block_hash.is_zero() && finalized_block_hash.is_zero() {
        return Ok(None);
    }

    Ok(Some(ForkchoiceState {
        head_block_hash: last_sent_head,
        safe_block_hash,
        finalized_block_hash,
    }))
}

/// Returns `target` if reth has not applied it yet and has it on its canonical chain.
///
/// Otherwise returns [`B256::ZERO`], which tells reth to keep its current value. Reth rejects an
/// update whose safe or finalized block is not canonical.
fn pending_hash<C: CanonicalChain>(
    target: B256,
    applied: Option<B256>,
    chain: &C,
) -> Result<B256, C::Error> {
    if applied == Some(target) || !chain.contains(target)? {
        return Ok(B256::ZERO);
    }
    Ok(target)
}

/// Sends `update` to the engine and returns whether it went through.
async fn send_forkchoice_update<E: ExecutionEngine>(engine: &E, update: ForkchoiceState) -> bool {
    if let Err(err) = engine.update_consensus_state(update).await {
        warn!(?err, "forkchoice_update failed");
        return false;
    }
    true
}

/// Takes chain updates from OL and sequencer/p2p and updates the chain in engine (reth).
///
/// Reth applies safe and finalized only from an update whose head it already has, so it drops
/// them from OL updates that arrive while it syncs. After every event, including reth's canonical
/// state notifications and a periodic recheck, the task resends the ones reth is missing.
async fn engine_control_task_inner<C, E, T>(
    mut preconf_rx: watch::Receiver<BlockNumHash>,
    mut consensus_rx: watch::Receiver<ConsensusHeads>,
    mut canon_state_rx: broadcast::Receiver<T>,
    chain: C,
    engine: E,
    recheck_interval: Duration,
) where
    C: CanonicalChain,
    E: ExecutionEngine,
    T: Clone,
{
    // The head of the last update sent to reth.
    let mut head_block_hash = chain.head_hash();

    // The watch channel counts its current value as seen. Without this, a node that restarts
    // after the OL tracker caught up would not resend the OL heads until the next OL update.
    consensus_rx.mark_changed();

    let mut recheck = time::interval(recheck_interval);

    loop {
        select! {
            res = consensus_rx.changed() => {
                if res.is_err() {
                    // tx dropped; exit task
                    warn!("consensus_rx channel closed; exiting");
                    return;
                }
                // got a consensus update from ol
                let consensus_state = consensus_rx.borrow_and_update().clone();
                match forkchoice_state_from_consensus(&consensus_state, head_block_hash, &chain) {
                    Ok(update) => {
                        if send_forkchoice_update(&engine, update).await {
                            head_block_hash = update.head_block_hash;
                        }
                    }
                    Err(err) => error!(?err, "failed to read the canonical chain"),
                }
            }
            res = preconf_rx.changed() => {
                if res.is_err() {
                    // tx dropped; exit task
                    warn!("preconf_rx channel closed; exiting");
                    return;
                }
                // got head block from sequencer / p2p
                let blocknumhash = *preconf_rx.borrow_and_update();
                let next_head_block_hash = B256::from_slice(blocknumhash.hash().as_slice());

                let update = ForkchoiceState {
                    head_block_hash: next_head_block_hash,
                    safe_block_hash: B256::ZERO,
                    finalized_block_hash: B256::ZERO,
                };
                if send_forkchoice_update(&engine, update).await {
                    head_block_hash = next_head_block_hash;
                }
            }
            res = canon_state_rx.recv() => {
                // Only the wakeup matters. A lagged receiver still means reth's chain moved.
                if let Err(broadcast::error::RecvError::Closed) = res {
                    warn!("canonical state channel closed; exiting");
                    return;
                }
            }
            // Reth sends no canonical state notification when a backfill run finishes, so the
            // tick catches a backfill that ends on the last sent head.
            _ = recheck.tick() => {}
        }

        let consensus_state = consensus_rx.borrow().clone();
        match safe_and_finalized_update(&consensus_state, head_block_hash, &chain) {
            Ok(Some(update)) => {
                debug!(
                    ?update,
                    "applying OL safe and finalized blocks reth is missing"
                );
                send_forkchoice_update(&engine, update).await;
            }
            Ok(None) => {}
            Err(err) => error!(?err, "failed to read the canonical chain"),
        }
    }
}

/// Creates an engine control task that processes chain updates from OL and sequencer.
pub fn create_engine_control_task<N: NodeTypesWithDB + ProviderNodeTypes, E: ExecutionEngine>(
    preconf_rx: watch::Receiver<BlockNumHash>,
    consensus_rx: watch::Receiver<ConsensusHeads>,
    provider: BlockchainProvider<N>,
    engine_control: E,
) -> impl Future<Output = ()> {
    let canon_state_rx = provider.subscribe_to_canonical_state();
    engine_control_task_inner(
        preconf_rx,
        consensus_rx,
        canon_state_rx,
        provider,
        engine_control,
        RECHECK_INTERVAL,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        convert::Infallible,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alpen_engine_api::ExecutionEngineError;
    use alpen_reth_node::AlpenBuiltPayload;
    use async_trait::async_trait;
    use strata_acct_types::Hash;
    use tokio::{
        task::{yield_now, JoinHandle},
        time::{sleep, timeout},
    };

    use super::*;

    /// Stands in for reth's chain state. Tests move it forward to simulate reth syncing.
    #[derive(Debug, Default)]
    struct FakeChainState {
        head: B256,
        safe: Option<B256>,
        finalized: Option<B256>,
        canonical: HashSet<B256>,
    }

    #[derive(Debug, Default, Clone)]
    struct FakeChain(Arc<Mutex<FakeChainState>>);

    impl FakeChain {
        /// Makes `blocks` canonical, with the last one as the head.
        fn sync_to(&self, blocks: &[B256]) {
            let mut state = self.0.lock().unwrap();
            state.canonical.extend(blocks);
            state.head = *blocks.last().unwrap();
        }
    }

    impl CanonicalChain for FakeChain {
        type Error = Infallible;

        fn head_hash(&self) -> B256 {
            self.0.lock().unwrap().head
        }

        fn safe_hash(&self) -> Option<B256> {
            self.0.lock().unwrap().safe
        }

        fn finalized_hash(&self) -> Option<B256> {
            self.0.lock().unwrap().finalized
        }

        fn contains(&self, blockhash: B256) -> Result<bool, Infallible> {
            Ok(self.0.lock().unwrap().canonical.contains(&blockhash))
        }
    }

    /// Records the forkchoice updates it receives and applies them to the chain like reth does.
    #[derive(Debug, Clone)]
    struct RecordingEngine {
        updates: Arc<Mutex<Vec<ForkchoiceState>>>,
        chain: FakeChain,
    }

    impl RecordingEngine {
        fn new(chain: FakeChain) -> Self {
            Self {
                updates: Arc::default(),
                chain,
            }
        }

        fn updates(&self) -> Vec<ForkchoiceState> {
            self.updates.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ExecutionEngine for RecordingEngine {
        type TEnginePayload = AlpenBuiltPayload;

        async fn submit_payload(
            &self,
            _payload: AlpenBuiltPayload,
        ) -> Result<(), ExecutionEngineError> {
            unreachable!("the engine control task never submits payloads")
        }

        async fn update_consensus_state(
            &self,
            state: ForkchoiceState,
        ) -> Result<(), ExecutionEngineError> {
            self.updates.lock().unwrap().push(state);

            let mut chain = self.chain.0.lock().unwrap();
            if chain.head == state.head_block_hash {
                if !state.safe_block_hash.is_zero() {
                    chain.safe = Some(state.safe_block_hash);
                }
                if !state.finalized_block_hash.is_zero() {
                    chain.finalized = Some(state.finalized_block_hash);
                }
            }
            Ok(())
        }
    }

    fn block(n: u8) -> B256 {
        B256::with_last_byte(n)
    }

    fn heads(confirmed: B256, finalized: B256) -> ConsensusHeads {
        ConsensusHeads {
            confirmed: Hash::from(confirmed.0),
            confirmed_epoch: 2,
            finalized: Hash::from(finalized.0),
            finalized_epoch: 1,
        }
    }

    fn fcu(head: B256, safe: B256, finalized: B256) -> ForkchoiceState {
        ForkchoiceState {
            head_block_hash: head,
            safe_block_hash: safe,
            finalized_block_hash: finalized,
        }
    }

    #[test]
    fn test_safe_and_finalized_update_waits_while_reth_syncs() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2), block(3)]);

        // The last update sent a head reth does not have yet.
        let update =
            safe_and_finalized_update(&heads(block(3), block(2)), block(9), &chain).unwrap();

        assert_eq!(update, None);
    }

    #[test]
    fn test_safe_and_finalized_update_sends_unapplied_heads() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2), block(3)]);

        let update =
            safe_and_finalized_update(&heads(block(3), block(2)), block(3), &chain).unwrap();

        assert_eq!(update, Some(fcu(block(3), block(3), block(2))));
    }

    #[test]
    fn test_safe_and_finalized_update_skips_applied_heads() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2), block(3)]);
        {
            let mut state = chain.0.lock().unwrap();
            state.safe = Some(block(3));
            state.finalized = Some(block(2));
        }

        let update =
            safe_and_finalized_update(&heads(block(3), block(2)), block(3), &chain).unwrap();

        assert_eq!(update, None);
    }

    #[test]
    fn test_safe_and_finalized_update_skips_non_canonical_heads() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2)]);

        // The safe block is on a fork reth does not follow; finalized is canonical.
        let update =
            safe_and_finalized_update(&heads(block(7), block(2)), block(2), &chain).unwrap();

        assert_eq!(update, Some(fcu(block(2), B256::ZERO, block(2))));
    }

    struct Harness {
        preconf_tx: watch::Sender<BlockNumHash>,
        consensus_tx: watch::Sender<ConsensusHeads>,
        canon_state_tx: broadcast::Sender<()>,
        chain: FakeChain,
        engine: RecordingEngine,
        task: JoinHandle<()>,
    }

    impl Harness {
        fn start(chain: FakeChain, initial_heads: ConsensusHeads) -> Self {
            let (preconf_tx, preconf_rx) =
                watch::channel(BlockNumHash::new(Hash::from(chain.head_hash().0), 0));
            let (consensus_tx, consensus_rx) = watch::channel(initial_heads);
            let (canon_state_tx, canon_state_rx) = broadcast::channel(16);
            let engine = RecordingEngine::new(chain.clone());
            let task = tokio::spawn(engine_control_task_inner(
                preconf_rx,
                consensus_rx,
                canon_state_rx,
                chain.clone(),
                engine.clone(),
                Duration::from_millis(10),
            ));
            Self {
                preconf_tx,
                consensus_tx,
                canon_state_tx,
                chain,
                engine,
                task,
            }
        }

        /// Waits until the engine has received `count` updates and returns them.
        async fn wait_for_updates(&self, count: usize) -> Vec<ForkchoiceState> {
            timeout(Duration::from_secs(5), async {
                loop {
                    let updates = self.engine.updates();
                    if updates.len() >= count {
                        return updates;
                    }
                    yield_now().await;
                }
            })
            .await
            .expect("engine did not receive the expected updates")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn test_finalized_applied_once_reth_catches_up_without_gossip() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1)]);
        let harness = Harness::start(chain, heads(block(1), block(1)));
        // Startup resends the OL heads, which reth applies.
        harness.wait_for_updates(1).await;

        // The OL moves ahead of reth. Reth answers SYNCING and keeps neither safe nor finalized.
        harness
            .consensus_tx
            .send(heads(block(3), block(2)))
            .unwrap();
        let updates = harness.wait_for_updates(2).await;
        assert_eq!(updates[1], fcu(block(3), block(3), block(2)));

        // Reth reaches the OL's safe block on its own and announces the new canonical chain.
        harness.chain.sync_to(&[block(2), block(3)]);
        harness.canon_state_tx.send(()).unwrap();

        let updates = harness.wait_for_updates(3).await;
        assert_eq!(updates[2], fcu(block(3), block(3), block(2)));
        assert_eq!(harness.chain.finalized_hash(), Some(block(2)));
    }

    #[tokio::test]
    async fn test_finalized_applied_once_reth_reaches_gossip_head() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2), block(3)]);
        let harness = Harness::start(chain, heads(block(1), block(1)));
        // Startup resends the OL heads, which reth applies.
        harness.wait_for_updates(1).await;

        // Gossip points reth at a block it does not have, so reth starts syncing towards it.
        harness
            .preconf_tx
            .send(BlockNumHash::new(Hash::from(block(5).0), 5))
            .unwrap();
        harness.wait_for_updates(2).await;

        // The OL update arrives while reth syncs. Reth answers SYNCING and drops it.
        harness
            .consensus_tx
            .send(heads(block(3), block(2)))
            .unwrap();
        let updates = harness.wait_for_updates(3).await;
        assert_eq!(updates[2], fcu(block(5), block(3), block(2)));

        // Reth commits block 4 on the way. Resending now would only be dropped again.
        harness.chain.sync_to(&[block(4)]);
        harness.canon_state_tx.send(()).unwrap();
        sleep(Duration::from_millis(50)).await;
        assert_eq!(harness.engine.updates().len(), 3);

        // Reth reaches the gossip head, so the dropped OL heads go out again.
        harness.chain.sync_to(&[block(5)]);
        harness.canon_state_tx.send(()).unwrap();
        let updates = harness.wait_for_updates(4).await;
        assert_eq!(updates[3], fcu(block(5), block(3), block(2)));
        assert_eq!(harness.chain.finalized_hash(), Some(block(2)));
    }

    #[tokio::test]
    async fn test_finalized_applied_after_backfill_without_notification() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1)]);
        let harness = Harness::start(chain, heads(block(1), block(1)));
        harness.wait_for_updates(1).await;

        harness
            .consensus_tx
            .send(heads(block(3), block(3)))
            .unwrap();
        let updates = harness.wait_for_updates(2).await;
        assert_eq!(updates[1], fcu(block(3), block(3), block(3)));

        // Reth backfills to the OL's head. Its backfill path sends no canonical state
        // notification, so only the recheck tick can pick this up.
        harness.chain.sync_to(&[block(2), block(3)]);

        let updates = harness.wait_for_updates(3).await;
        assert_eq!(updates[2], fcu(block(3), block(3), block(3)));
        assert_eq!(harness.chain.finalized_hash(), Some(block(3)));
    }

    #[tokio::test]
    async fn test_startup_resends_ol_heads() {
        let chain = FakeChain::default();
        chain.sync_to(&[block(1), block(2), block(3)]);

        let harness = Harness::start(chain, heads(block(3), block(2)));

        let updates = harness.wait_for_updates(1).await;
        assert_eq!(updates[0], fcu(block(3), block(3), block(2)));
    }
}
