use alpen_common::{Batch, BatchDaProvider, BatchProver, BatchStatus};
use alpen_storage_api::BatchStorage;
use eyre::Result;
use metrics::counter;
use tracing::{debug, warn};

use crate::{
    batch_lifecycle::{ctx::BatchLifecycleCtx, state::BatchLifecycleState},
    metrics::BATCH_TRANSITION_FAILURES,
};

/// Posts DA for as many sealed batches as possible (Sealed → DaPending).
///
/// Each batch is submitted as its own chunked-envelope tx set, so posting the
/// next batch does not depend on earlier batches reaching DA readiness. The
/// frontier advances while sealed batches have their state diffs available and
/// `post_batch_da` succeeds.
pub(crate) async fn try_advance_da_pending<D, P, S>(
    state: &mut BatchLifecycleState,
    latest_batch: &Batch,
    ctx: &BatchLifecycleCtx<D, P, S>,
) -> Result<()>
where
    D: BatchDaProvider,
    P: BatchProver,
    S: BatchStorage,
{
    loop {
        let batch_idx = state.da_pending().idx() + 1;
        if batch_idx > latest_batch.idx() {
            return Ok(());
        }

        let Some((batch, status)) = ctx.batch_storage.get_batch_by_idx(batch_idx).await? else {
            return Ok(());
        };

        match status {
            BatchStatus::Sealed => {
                let batch_id = batch.id();
                state.clock_mut().observe(batch_idx);

                debug!(batch_idx, ?batch_id, "Posting DA");

                let envelope_idx = match ctx.da_provider.post_batch_da(batch_id).await {
                    Ok(envelope_idx) => envelope_idx,
                    Err(e) => {
                        warn!(
                            batch_idx,
                            ?batch_id,
                            error = %e,
                            "failed to post batch DA; will retry next lifecycle tick"
                        );
                        counter!(BATCH_TRANSITION_FAILURES, "status" => "da_pending").increment(1);
                        return Ok(());
                    }
                };

                ctx.batch_storage
                    .update_batch_status(batch_id, BatchStatus::DaPending { envelope_idx })
                    .await
                    .inspect_err(|_| {
                        counter!(BATCH_TRANSITION_FAILURES, "status" => "da_pending").increment(1);
                    })?;

                state.advance_da_pending(batch_idx, batch_id);
                state.clock_mut().transition(batch_idx, "da_pending");
            }
            BatchStatus::DaPending { .. }
            | BatchStatus::DaComplete { .. }
            | BatchStatus::ProofPending { .. }
            | BatchStatus::ProofReady { .. }
            | BatchStatus::Genesis => {
                state.advance_da_pending(batch_idx, batch.id());
            }
        }
    }
}
