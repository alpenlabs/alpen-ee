use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use alpen_common::{BlockNumHash, ConsensusHeads, ExecutionEngine};
use reth_node_builder::NodeTypesWithDB;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    BlockHashReader, BlockNumReader, CanonStateSubscriptions, ProviderResult,
};
use tokio::{
    select,
    sync::{broadcast, watch},
};
use tracing::{debug, error, warn};

/// Read access to the parts of reth's chain state the engine control task uses.
///
/// Lets the tests stand in for [`BlockchainProvider`].
trait CanonicalChain {
    /// Returns the hash of reth's canonical head.
    fn head_hash(&self) -> B256;

    /// Returns the hash of the safe block reth has applied, if any.
    fn safe_hash(&self) -> Option<B256>;

    /// Returns the hash of the finalized block reth has applied, if any.
    fn finalized_hash(&self) -> Option<B256>;

    /// Returns whether `blockhash` is on reth's canonical chain.
    fn contains(&self, blockhash: B256) -> ProviderResult<bool>;
}

impl<N: NodeTypesWithDB + ProviderNodeTypes> CanonicalChain for BlockchainProvider<N> {
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

    fn contains(&self, blockhash: B256) -> ProviderResult<bool> {
        let Some(block_number) = self.block_number(blockhash)? else {
            return Ok(false);
        };
        let Some(canonical_blockhash) = self.block_hash(block_number)? else {
            return Ok(false);
        };
        Ok(blockhash == canonical_blockhash)
    }
}

fn forkchoice_state_from_consensus(
    consensus_state: &ConsensusHeads,
    head_block_hash: B256,
    chain: &impl CanonicalChain,
) -> ProviderResult<ForkchoiceState> {
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
fn safe_and_finalized_update(
    consensus_state: &ConsensusHeads,
    last_sent_head: B256,
    chain: &impl CanonicalChain,
) -> ProviderResult<Option<ForkchoiceState>> {
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
fn pending_hash(
    target: B256,
    applied: Option<B256>,
    chain: &impl CanonicalChain,
) -> ProviderResult<B256> {
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
/// state notifications, the task resends the ones reth is missing.
async fn engine_control_task_inner<C, E, T>(
    mut preconf_rx: watch::Receiver<BlockNumHash>,
    mut consensus_rx: watch::Receiver<ConsensusHeads>,
    mut canon_state_rx: broadcast::Receiver<T>,
    chain: C,
    engine: E,
) where
    C: CanonicalChain,
    E: ExecutionEngine,
    T: Clone,
{
    // The head of the last update sent to reth.
    let mut head_block_hash = chain.head_hash();

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
                    Err(err) => error!(?err, "failed to access blockchain provider"),
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
            Err(err) => error!(?err, "failed to access blockchain provider"),
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
    )
}
