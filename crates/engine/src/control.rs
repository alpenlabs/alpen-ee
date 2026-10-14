use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use alpen_common::{BlockNumHash, ConsensusHeads, ExecutionEngine};
use reth_node_builder::NodeTypesWithDB;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    BlockHashReader, BlockNumReader, ProviderResult,
};
use tokio::{select, sync::watch};
use tracing::{error, warn};

/// Read access to the parts of reth's chain state the engine control task uses.
///
/// Lets the tests stand in for [`BlockchainProvider`].
trait CanonicalChain {
    /// Returns the hash of reth's canonical head.
    fn head_hash(&self) -> B256;

    /// Returns whether `blockhash` is on reth's canonical chain.
    fn contains(&self, blockhash: B256) -> ProviderResult<bool>;
}

impl<N: NodeTypesWithDB + ProviderNodeTypes> CanonicalChain for BlockchainProvider<N> {
    fn head_hash(&self) -> B256 {
        self.canonical_in_memory_state().get_canonical_head().hash()
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

/// Takes chain updates from OL and sequencer/p2p and updates the chain in engine (reth).
async fn engine_control_task_inner<C: CanonicalChain, E: ExecutionEngine>(
    mut preconf_rx: watch::Receiver<BlockNumHash>,
    mut consensus_rx: watch::Receiver<ConsensusHeads>,
    chain: C,
    engine: E,
) {
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
                let update = match forkchoice_state_from_consensus(&consensus_state, head_block_hash, &chain) {
                    Ok(update) => update,
                    Err(err) => {
                        error!(?err, "failed to access blockchain provider");
                        continue;
                    }
                };

                if let Err(err) = engine.update_consensus_state(update).await {
                    warn!(?err, "forkchoice_update failed");
                    continue;
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
                if let Err(err) = engine.update_consensus_state(update).await {
                    warn!(?err, "forkchoice_update failed");
                    continue;
                }
                head_block_hash = next_head_block_hash;
            }
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
    engine_control_task_inner(preconf_rx, consensus_rx, provider, engine_control)
}
