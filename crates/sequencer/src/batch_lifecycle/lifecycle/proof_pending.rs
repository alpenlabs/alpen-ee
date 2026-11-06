use alpen_common::{Batch, BatchDaProvider, BatchProver, BatchStatus};
use alpen_storage_api::BatchStorage;
use eyre::Result;
use metrics::counter;
use tracing::debug;

use crate::{
    batch_lifecycle::{ctx::BatchLifecycleCtx, state::BatchLifecycleState},
    metrics::BATCH_TRANSITION_FAILURES,
};

/// Try to request proof for the next batch (DaComplete → ProofPending).
pub(crate) async fn try_advance_proof_pending<D, P, S>(
    state: &mut BatchLifecycleState,
    latest_batch: &Batch,
    ctx: &BatchLifecycleCtx<D, P, S>,
) -> Result<()>
where
    D: BatchDaProvider,
    P: BatchProver,
    S: BatchStorage,
{
    // Next batch to process is current frontier + 1
    let target_idx = state.proof_pending().idx() + 1;

    // If we're past the latest batch, nothing to do
    if target_idx > latest_batch.idx() {
        return Ok(());
    }

    let Some((batch, status)) = ctx
        .batch_storage
        .get_batch_by_idx(target_idx)
        .await
        .inspect_err(|_| {
            counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_pending").increment(1);
        })?
    else {
        return Ok(()); // Batch doesn't exist yet
    };

    match status {
        BatchStatus::Sealed | BatchStatus::DaPending { .. } => {
            // Not ready, no action
        }
        BatchStatus::DaComplete { da } => {
            // Request proof generation. If this fails, we retry in the next cycle.
            debug!(batch_idx = target_idx, batch_id = ?batch.id(), "Requesting proof");

            ctx.prover
                .request_proof_generation(batch.id())
                .await
                .inspect_err(|_| {
                    counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_pending").increment(1);
                })?;

            ctx.batch_storage
                .update_batch_status(batch.id(), BatchStatus::ProofPending { da })
                .await
                .inspect_err(|_| {
                    counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_pending").increment(1);
                })?;

            state.advance_proof_pending(target_idx, batch.id());
            state.clock_mut().transition(batch.id(), "proof_pending");
        }
        BatchStatus::ProofPending { .. }
        | BatchStatus::ProofReady { .. }
        | BatchStatus::Genesis => {
            // Already past this stage, advance frontier
            state.advance_proof_pending(target_idx, batch.id());
        }
    }

    Ok(())
}
