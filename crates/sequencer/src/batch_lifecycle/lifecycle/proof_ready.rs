use alpen_common::{Batch, BatchDaProvider, BatchProver, BatchStatus, ProofGenerationStatus};
use alpen_storage_api::BatchStorage;
use eyre::Result;
use metrics::counter;
use tracing::{debug, debug_span, error, warn, Instrument};

use crate::{
    batch_lifecycle::{ctx::BatchLifecycleCtx, state::BatchLifecycleState},
    metrics::BATCH_TRANSITION_FAILURES,
};

/// Try to complete proof for the next batch (ProofPending → ProofReady).
pub(crate) async fn try_advance_proof_ready<D, P, S>(
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
    let target_idx = state.proof_ready().idx() + 1;

    // If we're past the latest batch, nothing to do
    if target_idx > latest_batch.idx() {
        return Ok(());
    }

    let Some((batch, status)) = ctx.batch_storage.get_batch_by_idx(target_idx).await? else {
        return Ok(()); // Batch doesn't exist yet
    };

    match status {
        BatchStatus::Sealed | BatchStatus::DaPending { .. } | BatchStatus::DaComplete { .. } => {
            // Not ready, no action
        }
        BatchStatus::ProofPending { da } => {
            let batch_id = batch.id();
            let proof_ready_span = debug_span!(
                "alpen_ee_proof_ready",
                batch_idx = target_idx,
                ?batch_id,
                prev_block = %batch.prev_block(),
                last_block = %batch.last_block(),
            );

            async {
                // Check proof status
                match ctx.prover.check_proof_status(batch_id).await? {
                    ProofGenerationStatus::Ready { proof_id } => {
                        debug!(proof_id = %proof_id, "Proof ready");

                        ctx.batch_storage
                            .update_batch_status(
                                batch_id,
                                BatchStatus::ProofReady {
                                    da,
                                    proof: proof_id,
                                },
                            )
                            .await
                            .inspect_err(|_| {
                                counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_ready")
                                    .increment(1);
                            })?;

                        // Notify watchers
                        let _ = ctx.proof_ready_tx.send(Some(batch_id));

                        state.advance_proof_ready(target_idx, batch_id);
                        state.clock_mut().transition(target_idx, "proof_ready");
                    }

                    ProofGenerationStatus::Failed { reason } => {
                        // CRITICAL: Manual intervention required
                        error!(
                            %reason,
                            "CRITICAL: Proof generation failed - manual intervention required. \
                             Batch is stuck in ProofPending state."
                        );
                        counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_ready").increment(1);
                        // Stay at frontier - manual intervention required
                    }

                    ProofGenerationStatus::Pending => {
                        // Still waiting, no action
                    }

                    ProofGenerationStatus::NotStarted => {
                        // We've marked the batch as proof pending, but prover says proof generation has
                        // not started. Try to re-request proof generation and hope for the best.
                        warn!(
                            "Expected proof generation to have been started. Retrying proof generation"
                        );
                        counter!(BATCH_TRANSITION_FAILURES, "status" => "proof_ready").increment(1);

                        ctx.prover.request_proof_generation(batch_id).await?;
                    }
                }

                Ok::<(), eyre::Report>(())
            }
            .instrument(proof_ready_span)
            .await?;
        }
        BatchStatus::ProofReady { .. } => {
            // The prover's receipt hook flipped the status before this task
            // asked, so the batch completes its last stage here.
            state.advance_proof_ready(target_idx, batch.id());
            state.clock_mut().transition(target_idx, "proof_ready");
        }
        BatchStatus::Genesis => {
            // Already complete, advance frontier
            state.advance_proof_ready(target_idx, batch.id());
        }
    }

    Ok(())
}
