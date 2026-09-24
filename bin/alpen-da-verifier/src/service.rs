//! Service framework integration for EE DA verification.

use std::marker::PhantomData;

use serde::Serialize;
use strata_identifiers::L1Height;
use strata_service::{AsyncService, Response, Service, ServiceState};
use tracing::warn;

use crate::{context::DaVerifierContext, state::DaVerifierServiceState};

/// Current EE DA recovery progress.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DaVerifierStatus {
    /// Next L1 height DA recovery will scan.
    pub(crate) next_l1_height: L1Height,

    /// Latest reorg-safe L1 tip observed, absent until the chain is deeper
    /// than the configured safe depth.
    pub(crate) reorg_safe_tip: Option<L1Height>,
}

impl<C: DaVerifierContext> ServiceState for DaVerifierServiceState<C> {
    fn name(&self) -> &str {
        "da_verifier"
    }

    fn span_prefix(&self) -> &str {
        "da_verifier"
    }
}

/// EE DA verifier service marker.
#[derive(Debug)]
pub(crate) struct DaVerifierService<C: DaVerifierContext>(PhantomData<C>);

impl<C: DaVerifierContext> Service for DaVerifierService<C> {
    type State = DaVerifierServiceState<C>;
    type Msg = ();
    type Status = DaVerifierStatus;

    fn get_status(state: &Self::State) -> Self::Status {
        DaVerifierStatus {
            next_l1_height: state.next_l1_height(),
            reorg_safe_tip: state.reorg_safe_tip(),
        }
    }
}

impl<C: DaVerifierContext> AsyncService for DaVerifierService<C> {
    async fn process_input(state: &mut Self::State, _input: ()) -> anyhow::Result<Response> {
        match state.handle_tick().await {
            Ok(()) => Ok(Response::Continue),
            Err(error) if error.is_recoverable() => {
                warn!(%error, "EE DA verification cycle will retry");
                Ok(Response::Continue)
            }
            Err(error) => Err(error.into()),
        }
    }
}
