//! Mutable state and state transitions for the DA fee-rate service.

use std::{fmt, time::Duration};

use alpen_reth_node::{da_fee_rate_channel, DaFeeRateHandle, DaFeeRateUpdater};
use thiserror::Error;
use tokio::time::{error::Elapsed, timeout, Instant};

use super::{
    policy::{DaFeeRatePolicy, DaFeeRatePolicyError},
    rate::{AdjustedRate, AffineAdjustment, AffineAdjustmentError, PolicyRate},
};
use crate::config::DaFeeRateConfig;

/// Holds the mutable state owned by [`super::service::DaFeeRateService`].
pub(super) struct DaFeeRateServiceState {
    /// Resolves unadjusted rates from the configured source.
    pub(super) policy: Box<dyn DaFeeRatePolicy>,
    /// Applies the operator's multiplier and offset before publication.
    adjustment: AffineAdjustment,
    /// Owns the capability to publish a new cached rate.
    updater: DaFeeRateUpdater,
    /// Provides read-only access to the currently published rate.
    pub(super) handle: DaFeeRateHandle,
    /// Inclusive operator-approved bounds for adjusted external rates.
    rate_bounds: Option<(u64, u64)>,
    /// Bounds a complete policy fetch as a final watchdog.
    pub(super) policy_fetch_timeout: Duration,
    /// Controls how often the policy is queried.
    pub(super) refresh_interval: Duration,
    /// Marks the service stale after this long without a successful publication.
    pub(super) stale_after: Duration,
    /// Tracks freshness from the latest successful publication.
    pub(super) last_success_at: Instant,
    /// Records whether the service has crossed its stale threshold.
    pub(super) is_stale: bool,
}

impl fmt::Debug for DaFeeRateServiceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaFeeRateServiceState")
            .field("adjustment", &self.adjustment)
            .field("current_rate", &self.handle.current_rate())
            .field("rate_bounds", &self.rate_bounds)
            .field("policy_fetch_timeout", &self.policy_fetch_timeout)
            .field("refresh_interval", &self.refresh_interval)
            .field("stale_after", &self.stale_after)
            .field("last_success_at", &self.last_success_at)
            .field("is_stale", &self.is_stale)
            .finish_non_exhaustive()
    }
}

/// Reports why an initial or periodic rate resolution failed.
#[derive(Debug, Error)]
pub(super) enum RateResolutionError {
    /// The configured policy could not resolve a rate.
    #[error(transparent)]
    Policy(#[from] DaFeeRatePolicyError),
    /// The policy query exceeded the configured timeout.
    #[error("DA fee-rate policy fetch timed out")]
    Timeout,
    /// The operator adjustment could not represent the final rate.
    #[error(transparent)]
    Adjustment(#[from] AffineAdjustmentError),
    /// The adjusted rate falls outside the operator-approved range.
    #[error("adjusted DA fee rate {rate} is outside configured range {min}..={max}")]
    OutsideBounds { rate: u64, min: u64, max: u64 },
}

impl RateResolutionError {
    /// Returns whether an outer watchdog or configured source timeout expired.
    pub(super) fn is_timeout(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Policy(DaFeeRatePolicyError::Source(error)) => error.is::<Elapsed>(),
            Self::Policy(_) | Self::Adjustment(_) | Self::OutsideBounds { .. } => false,
        }
    }
}

/// Describes a successfully published policy rate.
#[derive(Debug)]
pub(super) struct RateUpdate {
    /// Contains the rate returned directly by the policy.
    pub(super) policy_rate: PolicyRate,
    /// Contains the final rate made visible to payload construction.
    pub(super) adjusted_rate: AdjustedRate,
    /// Indicates whether publication changed the cached numeric value.
    pub(super) changed: bool,
    /// Indicates whether this success transitioned the service from stale to fresh.
    pub(super) recovered: bool,
}

impl DaFeeRateServiceState {
    /// Resolves an initial policy rate before creating the payload handle.
    pub(super) async fn initialize(
        policy: Box<dyn DaFeeRatePolicy>,
        config: &DaFeeRateConfig,
    ) -> Result<Self, RateResolutionError> {
        let policy_rate = fetch_policy_rate(policy.as_ref(), config.policy_fetch_timeout()).await?;
        Self::new(policy, config, policy_rate)
    }

    /// Builds initialized state from a policy rate that has already been resolved.
    pub(super) fn new(
        policy: Box<dyn DaFeeRatePolicy>,
        config: &DaFeeRateConfig,
        policy_rate: PolicyRate,
    ) -> Result<Self, RateResolutionError> {
        let adjustment =
            AffineAdjustment::new(config.multiplier_bps(), config.offset_wei_per_byte());
        let rate_bounds = config.rate_bounds();
        let adjusted_rate = adjust_rate(adjustment, policy_rate, rate_bounds)?;
        let (updater, handle) = da_fee_rate_channel(adjusted_rate.wei_per_byte());

        Ok(Self {
            policy,
            adjustment,
            updater,
            handle,
            rate_bounds,
            policy_fetch_timeout: config.policy_fetch_timeout(),
            refresh_interval: Duration::from_secs(config.refresh_interval_seconds().get()),
            stale_after: Duration::from_secs(config.stale_after_seconds().get()),
            last_success_at: Instant::now(),
            is_stale: false,
        })
    }

    /// Adjusts and publishes a successfully fetched policy rate.
    ///
    /// Successful publication refreshes [`Self::last_success_at`] and clears
    /// [`Self::is_stale`]. The returned recovery flag preserves whether the
    /// service was stale immediately before that transition.
    pub(super) fn apply_policy_rate(
        &mut self,
        now: Instant,
        rate: PolicyRate,
    ) -> Result<RateUpdate, RateResolutionError> {
        let adjusted_rate = adjust_rate(self.adjustment, rate, self.rate_bounds)?;
        let changed =
            self.updater.publish(adjusted_rate.wei_per_byte()) != adjusted_rate.wei_per_byte();

        let recovered = self.is_stale;
        self.is_stale = false;
        self.last_success_at = now;

        Ok(RateUpdate {
            policy_rate: rate,
            adjusted_rate,
            changed,
            recovered,
        })
    }

    /// Marks the service stale and returns `true` only on the fresh-to-stale transition.
    pub(super) fn mark_stale_if_needed(&mut self, now: Instant) -> bool {
        if !self.is_stale && now.duration_since(self.last_success_at) >= self.stale_after {
            self.is_stale = true;
            true
        } else {
            false
        }
    }
}

fn adjust_rate(
    adjustment: AffineAdjustment,
    policy_rate: PolicyRate,
    rate_bounds: Option<(u64, u64)>,
) -> Result<AdjustedRate, RateResolutionError> {
    let adjusted_rate = adjustment.apply(policy_rate)?;
    let rate = adjusted_rate.wei_per_byte();
    if let Some((min, max)) = rate_bounds {
        if !(min..=max).contains(&rate) {
            return Err(RateResolutionError::OutsideBounds { rate, min, max });
        }
    }
    Ok(adjusted_rate)
}

/// Fetches a policy rate within the service's source timeout.
pub(super) async fn fetch_policy_rate(
    policy: &dyn DaFeeRatePolicy,
    fetch_timeout: Duration,
) -> Result<PolicyRate, RateResolutionError> {
    match timeout(fetch_timeout, policy.fetch_rate()).await {
        Ok(result) => result.map_err(RateResolutionError::Policy),
        Err(_) => Err(RateResolutionError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::sequencer::da_fee_rate::test_support::{
        rate_config, service_config, service_state_with_policy, writer_backed_policy_config,
        PendingPolicy, ScriptedPolicy,
    };

    #[tokio::test]
    async fn initialization_requires_a_successful_policy_fetch() {
        let error = DaFeeRateServiceState::initialize(
            Box::new(ScriptedPolicy::new([Err("unavailable")])),
            &service_config(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, RateResolutionError::Policy(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn initialization_times_out_when_the_policy_does_not_respond() {
        let error = DaFeeRateServiceState::initialize(Box::new(PendingPolicy), &service_config())
            .await
            .unwrap_err();

        assert!(matches!(error, RateResolutionError::Timeout));
    }

    #[tokio::test]
    async fn initialization_rejects_an_adjusted_rate_that_overflows() {
        let config = rate_config(writer_backed_policy_config(), 5, 10, 10_001, 0);
        let error = DaFeeRateServiceState::initialize(
            Box::new(ScriptedPolicy::new([Ok(PolicyRate::new(u64::MAX))])),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, RateResolutionError::Adjustment(_)));
    }

    #[test]
    fn initialization_rejects_a_rate_below_the_configured_minimum() {
        let config: DaFeeRateConfig = toml::from_str(
            r#"
            policy = "writer_backed"
            refresh_interval_seconds = 5
            stale_after_seconds = 10
            min_rate_wei_per_byte = 10
            max_rate_wei_per_byte = 20
            "#,
        )
        .unwrap();

        let error = DaFeeRateServiceState::new(
            Box::new(ScriptedPolicy::new([])),
            &config,
            PolicyRate::new(9),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RateResolutionError::OutsideBounds { rate: 9, .. }
        ));
    }

    #[test]
    fn successful_fetch_publishes_only_the_fully_adjusted_rate() {
        let config = rate_config(writer_backed_policy_config(), 5, 10, 15_000, 3);
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), config, 5);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(2);

        let update = state.apply_policy_rate(now, PolicyRate::new(7)).unwrap();

        assert_eq!(update.policy_rate.wei_per_byte(), 7);
        assert_eq!(update.adjusted_rate.wei_per_byte(), 14);
        assert!(update.changed);
        assert!(!update.recovered);
        assert_eq!(state.handle.current_rate(), 14);
        assert_eq!(state.last_success_at, now);
        assert!(!state.is_stale);
    }

    #[test]
    fn unchanged_success_still_refreshes_freshness() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = state.apply_policy_rate(now, PolicyRate::new(10)).unwrap();

        assert!(!update.changed);
        assert!(!update.recovered);
        assert_eq!(state.handle.current_rate(), 10);
        assert_eq!(state.last_success_at, now);
        assert!(!state.is_stale);
    }

    #[test]
    fn adjustment_failure_retains_the_current_rate_and_success_time() {
        let config = rate_config(writer_backed_policy_config(), 5, 10, 10_001, 0);
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), config, 1);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = state.apply_policy_rate(now, PolicyRate::new(u64::MAX));

        assert!(matches!(update, Err(RateResolutionError::Adjustment(_))));
        assert_eq!(state.handle.current_rate(), 2);
        assert_eq!(state.last_success_at, previous_success);
    }

    #[test]
    fn out_of_bounds_refresh_retains_the_current_rate_and_success_time() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = state.apply_policy_rate(now, PolicyRate::new(u64::MAX));

        assert!(matches!(
            update,
            Err(RateResolutionError::OutsideBounds { .. })
        ));
        assert_eq!(state.handle.current_rate(), 10);
        assert_eq!(state.last_success_at, previous_success);
    }

    #[test]
    fn stale_boundary_and_successful_recovery() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let activated_at = state.last_success_at;

        assert!(state.mark_stale_if_needed(activated_at + Duration::from_secs(10)));
        assert!(state.is_stale);

        let recovered_at = activated_at + Duration::from_secs(11);
        let update = state
            .apply_policy_rate(recovered_at, PolicyRate::new(10))
            .unwrap();

        assert!(!update.changed);
        assert!(update.recovered);
        assert!(!state.is_stale);
        assert_eq!(state.last_success_at, recovered_at);
    }
}
