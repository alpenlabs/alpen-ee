//! Prunes per-block proof witnesses and accessed-state records below finality.
//!
//! The sequencer stores a depth-0 proof witness and an accessed-state record
//! for every block it builds. Both are proving inputs: the chunk prover reads
//! the witnesses of a chunk's blocks, the account proof reads the accessed
//! state of a batch's range. A block at or below the finalized height sits in
//! an update the OL accepted with its proof, and the node never reorgs below
//! it, so its rows are dead weight. Pruning a height also frees the rows of
//! the forks at it, which is how a reorged-out block's rows are freed (a
//! block that reorgs back needs its witness, so the revert leaves them);
//! deleting a block through the store frees its rows with it. Nothing else
//! does, and on a busy chain these rows are most of the store's growth.
//!
//! The task wakes on an interval, reads the finalized height, keeps
//! `keep_blocks` of margin below it, and deletes the rows of every height
//! below that in bounded transactions, so a long backlog (a store that never
//! pruned) drains over a few ticks without holding the single writer.
//!
//! Where pruning left off is not persisted. On start the task finds the
//! first height that still holds a witness by binary search over the
//! finalized chain, which assumes the pruned heights form a prefix. A hole
//! above that prefix (a block that never had a witness) only costs the rows
//! below it, which an operator can drop by hand with `dbconsole`.

use std::{sync::Arc, time::Duration};

use alpen_storage_api::{AccessedStateStore, BlockWitnessStore, ExecBlockStorage};
use alpen_storage_store::{NodeStorage, PrunedWitnesses};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::WitnessPruningConfig;

/// Runs until the task executor drops it.
pub(crate) async fn run(storage: Arc<NodeStorage>, config: WitnessPruningConfig) {
    let mut tick = interval(Duration::from_secs(config.interval_secs.get()));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first height that may still hold rows; found on the first tick.
    let mut next_height: Option<u64> = None;
    loop {
        tick.tick().await;
        let finalized = match storage.best_finalized_block().await {
            Ok(Some(block)) => block.blocknum(),
            Ok(None) => continue,
            Err(error) => {
                warn!(%error, "witness pruner: reading the finalized block failed");
                continue;
            }
        };
        // Everything strictly below `end` can go.
        let end = finalized.saturating_sub(config.keep_blocks);
        let start = match next_height {
            Some(height) => height,
            None => match first_height_with_rows(storage.as_ref(), end).await {
                Ok(height) => {
                    debug!(height, finalized, "witness pruner: resuming");
                    height
                }
                Err(error) => {
                    warn!(%error, "witness pruner: locating the pruned prefix failed");
                    continue;
                }
            },
        };
        if start >= end {
            next_height = Some(start);
            continue;
        }

        let mut cursor = start;
        let mut pruned = PrunedWitnesses::default();
        for _ in 0..config.max_txns_per_tick.get() {
            if cursor >= end {
                break;
            }
            let batch_end = cursor.saturating_add(config.heights_per_txn.get()).min(end);
            match storage.prune_proof_witnesses(cursor, batch_end).await {
                Ok(removed) => {
                    pruned += removed;
                    cursor = batch_end;
                }
                Err(error) => {
                    warn!(%error, from = cursor, to = batch_end, "witness pruner: delete failed; retrying next tick");
                    break;
                }
            }
        }
        next_height = Some(cursor);
        if !pruned.is_empty() {
            info!(
                from = start,
                to = cursor,
                finalized,
                witnesses = pruned.witnesses,
                accessed_states = pruned.accessed_states,
                backlog = end.saturating_sub(cursor),
                "pruned proof witnesses below finality"
            );
        }
    }
}

/// The lowest height in `0..end` whose canonical block still has a witness or
/// an accessed-state record, or `end` when none has. Binary search: pruning
/// always removes a prefix, so presence is monotone over the finalized chain.
async fn first_height_with_rows(storage: &NodeStorage, end: u64) -> eyre::Result<u64> {
    let (mut lo, mut hi) = (0u64, end);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if has_rows(storage, mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

async fn has_rows(storage: &NodeStorage, height: u64) -> eyre::Result<bool> {
    let Some(block) = storage.get_finalized_block_at_height(height).await? else {
        return Ok(false);
    };
    let hash = block.blockhash();
    Ok(storage.get_block_witness(hash).await?.is_some()
        || storage.get_block_accessed_state(hash).await?.is_some())
}
