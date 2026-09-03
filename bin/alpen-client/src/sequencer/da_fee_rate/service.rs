//! `strata_service` lifecycle and periodic input handling for DA fee rates.

use alpen_reth_node::DaFeeRateHandle;
use metrics::{counter, gauge};
use serde::Serialize;
use strata_service::{
    AsyncExecutor, AsyncService, AsyncServiceInput, DumbTickHandle, DumbTickingInput, Response,
    Service, ServiceBuilder, ServiceMonitor, ServiceState,
};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::state::{fetch_policy_rate, DaFeeRateServiceState, RateResolutionError, RateUpdate};

const CURRENT_RATE_METRIC: &str = "alpen_da_fee_rate_current_wei_per_byte";
const STALE_METRIC: &str = "alpen_da_fee_rate_stale";
const REFRESHES_METRIC: &str = "alpen_da_fee_rate_refreshes_total";

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
    /// Keeps the periodic input alive for the lifetime of this handle.
    _tick_handle: DumbTickHandle,
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
}

/// Launches periodic DA fee-rate refreshes through the service framework.
pub(super) async fn launch(
    state: DaFeeRateServiceState,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<DaFeeRateServiceHandle> {
    let rate_handle = state.handle.clone();
    set_current_rate_metric(rate_handle.current_rate());
    gauge!(STALE_METRIC).set(u8::from(state.is_stale));
    info!(
        initial_rate_wei_per_byte = rate_handle.current_rate(),
        "DA fee-rate service initialized"
    );

    let (tick_handle, mut input) = DumbTickingInput::new(state.refresh_interval);
    // Tokio intervals emit once immediately. Initialization already fetched a
    // rate, so consume that tick and let the service first refresh one full
    // interval from now.
    input
        .recv_next()
        .await?
        .expect("a newly created ticking input must emit its initial tick");
    let monitor = ServiceBuilder::<DaFeeRateService, _>::new()
        .with_state(state)
        .with_input(input)
        .launch_async("da_fee_rate", executor)
        .await?;

    Ok(DaFeeRateServiceHandle {
        rate_handle,
        monitor,
        _tick_handle: tick_handle,
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
        let fetch_result = fetch_policy_rate(state.policy.as_ref()).await;
        let now = Instant::now();

        match fetch_result.and_then(|rate| state.apply_policy_rate(now, rate)) {
            Ok(update) => {
                log_update(&update);
                set_current_rate_metric(update.adjusted_rate.wei_per_byte());
                gauge!(STALE_METRIC).set(0);
                counter!(REFRESHES_METRIC, "outcome" => "success").increment(1);
            }
            Err(error) => {
                let outcome = if matches!(&error, RateResolutionError::Timeout) {
                    "timeout"
                } else {
                    "failure"
                };
                counter!(REFRESHES_METRIC, "outcome" => outcome).increment(1);
                warn!(
                    %error,
                    current_rate_wei_per_byte = state.handle.current_rate(),
                    "DA fee-rate refresh failed; retaining the last usable rate"
                );
            }
        }

        if state.mark_stale_if_needed(now) {
            gauge!(STALE_METRIC).set(1);
            warn!(
                current_rate_wei_per_byte = state.handle.current_rate(),
                stale_after_seconds = state.stale_after.as_secs(),
                "DA fee rate became stale; retaining the last usable rate"
            );
        }

        Ok(Response::Continue)
    }
}

fn set_current_rate_metric(rate_wei_per_byte: u64) {
    #[expect(
        clippy::cast_precision_loss,
        reason = "metrics gauges represent values as f64"
    )]
    gauge!(CURRENT_RATE_METRIC).set(rate_wei_per_byte as f64);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{task::yield_now, time::advance};

    use super::*;
    use crate::sequencer::da_fee_rate::{
        rate::PolicyRate,
        state::POLICY_FETCH_TIMEOUT,
        test_support::{service_config, service_state_with_policy, PendingPolicy, ScriptedPolicy},
    };

    #[test]
    fn service_status_reports_current_rate_and_freshness() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let stale_at = state.last_success_at + state.stale_after + Duration::from_nanos(1);
        assert!(state.mark_stale_if_needed(stale_at));

        assert_eq!(
            DaFeeRateService::get_status(&state),
            DaFeeRateStatus {
                current_rate_wei_per_byte: 10,
                is_stale: true,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_ticks_retain_the_rate_and_eventually_mark_it_stale() {
        let mut state = service_state_with_policy(
            ScriptedPolicy::new([Err("unavailable"), Err("still unavailable")]),
            service_config(),
            10,
        );

        let response = DaFeeRateService::process_input(&mut state, ())
            .await
            .unwrap();
        assert_eq!(response, Response::Continue);
        assert_eq!(state.handle.current_rate(), 10);
        assert!(!state.is_stale);

        advance(Duration::from_secs(10) + Duration::from_nanos(1)).await;
        let response = DaFeeRateService::process_input(&mut state, ())
            .await
            .unwrap();
        assert_eq!(response, Response::Continue);
        assert_eq!(state.handle.current_rate(), 10);
        assert!(state.is_stale);
    }

    #[tokio::test]
    async fn service_tick_publishes_successful_fetch() {
        let mut state = service_state_with_policy(
            ScriptedPolicy::new([Ok(PolicyRate::new(23))]),
            service_config(),
            10,
        );

        let response = DaFeeRateService::process_input(&mut state, ())
            .await
            .unwrap();

        assert_eq!(response, Response::Continue);
        assert_eq!(state.handle.current_rate(), 23);
    }

    #[tokio::test(start_paused = true)]
    async fn service_tick_times_out_without_replacing_last_successful_rate() {
        let mut state = service_state_with_policy(PendingPolicy, service_config(), 10);
        let task = tokio::spawn(async move {
            let response = DaFeeRateService::process_input(&mut state, ()).await;
            (response, state.handle.current_rate())
        });

        yield_now().await;
        advance(POLICY_FETCH_TIMEOUT).await;
        let (response, current_rate) = task.await.unwrap();

        assert_eq!(response.unwrap(), Response::Continue);
        assert_eq!(current_rate, 10);
    }
}
