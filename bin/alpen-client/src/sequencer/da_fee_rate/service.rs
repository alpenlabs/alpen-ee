//! `strata_service` lifecycle and periodic input handling for DA fee rates.

use alpen_reth_node::DaFeeRateHandle;
use serde::Serialize;
use strata_service::{
    AsyncExecutor, AsyncService, DumbTickHandle, DumbTickingInput, Response, Service,
    ServiceBuilder, ServiceMonitor, ServiceState,
};
use tokio::time::{timeout, Instant};
use tracing::{debug, info, warn};

use super::state::{DaFeeRateServiceState, RateUpdate, RefreshError};

/// Runs [`DaFeeRateServiceState`] from periodic framework ticks.
#[derive(Debug)]
pub(super) struct DaFeeRateService;

/// Exposes the DA fee-rate state tracked by the service framework.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DaFeeRateStatus {
    /// Contains the rate currently visible to payload construction.
    pub(crate) current_rate_wei_per_byte: u64,
    /// Indicates that no policy rate has been published within the configured threshold.
    pub(crate) is_stale: bool,
}

/// Keeps service lifecycle and health separate from payload rate access.
#[derive(Debug)]
pub(crate) struct DaFeeRateServiceHandle {
    rate_handle: DaFeeRateHandle,
    monitor: ServiceMonitor<DaFeeRateStatus>,
    tick_handle: DumbTickHandle,
}

impl DaFeeRateServiceHandle {
    /// Returns the non-blocking rate handle consumed by payload construction.
    pub(crate) fn rate_handle(&self) -> DaFeeRateHandle {
        self.rate_handle.clone()
    }

    /// Returns the service framework monitor for health observation.
    #[expect(dead_code, reason = "part of service handle API, not yet consumed")]
    pub(crate) fn monitor(&self) -> &ServiceMonitor<DaFeeRateStatus> {
        &self.monitor
    }

    /// Stops refresh ticks and lets the service exit normally.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "part of service handle API, not yet consumed")
    )]
    pub(crate) fn stop(self) -> bool {
        self.tick_handle.stop()
    }
}

/// Starts periodic DA fee-rate refreshes through the service framework.
pub(crate) async fn start(
    mut state: DaFeeRateServiceState,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<DaFeeRateServiceHandle> {
    state.last_success_at = Instant::now();
    let rate_handle = state.handle.clone();
    info!(
        fallback_rate_wei_per_byte = rate_handle.current_rate(),
        "DA fee-rate service activated fallback"
    );

    // DumbTickingInput emits an immediate first tick and skips missed ticks,
    // preserving the previous refresh loop's refresh cadence.
    let (tick_handle, input) = DumbTickingInput::new(state.refresh_interval);
    let monitor = ServiceBuilder::<DaFeeRateService, _>::new()
        .with_state(state)
        .with_input(input)
        .launch_async("da_fee_rate", executor)
        .await?;

    Ok(DaFeeRateServiceHandle {
        rate_handle,
        monitor,
        tick_handle,
    })
}

impl ServiceState for DaFeeRateServiceState {
    fn name(&self) -> &str {
        "da_fee_rate"
    }

    fn span_prefix(&self) -> &str {
        "da_fee_rate"
    }
}

impl Service for DaFeeRateService {
    type State = DaFeeRateServiceState;
    type Msg = ();
    type Status = DaFeeRateStatus;

    fn get_status(state: &Self::State) -> Self::Status {
        DaFeeRateStatus {
            current_rate_wei_per_byte: state.handle.current_rate(),
            is_stale: state.is_stale,
        }
    }
}

impl AsyncService for DaFeeRateService {
    async fn process_input(state: &mut Self::State, _input: Self::Msg) -> anyhow::Result<Response> {
        let fetch_result = match timeout(state.fetch_timeout, state.policy.fetch_rate()).await {
            Ok(result) => result.map_err(RefreshError::Policy),
            Err(_) => Err(RefreshError::Timeout),
        };
        let now = Instant::now();

        match fetch_result.and_then(|rate| state.apply_policy_rate(now, rate)) {
            Ok(update) => log_update(&update),
            Err(error) => warn!(
                %error,
                current_rate_wei_per_byte = state.handle.current_rate(),
                "DA fee-rate refresh failed; retaining the last usable rate"
            ),
        }

        if state.mark_stale_if_needed(now) {
            warn!(
                current_rate_wei_per_byte = state.handle.current_rate(),
                stale_after_seconds = state.stale_after.as_secs(),
                "DA fee rate became stale; retaining the last usable rate"
            );
        }

        Ok(Response::Continue)
    }
}

/// Records a successful rate publication and any state transition it caused.
fn log_update(update: &RateUpdate) {
    let RateUpdate {
        policy_rate,
        adjusted_rate,
        changed,
        recovered,
    } = update;

    if *changed || *recovered {
        info!(
            policy_rate_wei_per_byte = policy_rate.wei_per_byte(),
            adjusted_rate_wei_per_byte = adjusted_rate.wei_per_byte(),
            changed = *changed,
            recovered = *recovered,
            "DA fee rate refreshed"
        );
    } else {
        debug!(
            policy_rate_wei_per_byte = policy_rate.wei_per_byte(),
            adjusted_rate_wei_per_byte = adjusted_rate.wei_per_byte(),
            changed = *changed,
            recovered = *recovered,
            "DA fee rate refreshed"
        );
    }
}
