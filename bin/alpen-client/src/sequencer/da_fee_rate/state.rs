//! Mutable state and state transitions for the DA fee-rate service.

use std::{fmt, time::Duration};

use alpen_reth_node::{da_fee_rate_channel, DaFeeRateHandle, DaFeeRateUpdater};
use thiserror::Error;
use tokio::time::{timeout, Instant};

use super::{
    AdjustedRate, AffineAdjustment, AffineAdjustmentError, DaFeeRatePolicy, DaFeeRatePolicyError,
    PolicyRate,
};
use crate::config::DaFeeRateConfig;

pub(super) const POLICY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Holds the mutable state owned by [`super::service::DaFeeRateService`].
pub(crate) struct DaFeeRateServiceState {
    /// Resolves unadjusted rates from the configured source.
    pub(super) policy: Box<dyn DaFeeRatePolicy>,
    /// Applies the operator's multiplier and offset before publication.
    adjustment: AffineAdjustment,
    /// Owns the capability to publish a new cached rate.
    updater: DaFeeRateUpdater,
    /// Provides read-only access to the currently published rate.
    pub(super) handle: DaFeeRateHandle,
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
            .field("refresh_interval", &self.refresh_interval)
            .field("stale_after", &self.stale_after)
            .field("last_success_at", &self.last_success_at)
            .field("is_stale", &self.is_stale)
            .finish_non_exhaustive()
    }
}

/// Reports why an initial or periodic rate resolution failed.
#[derive(Debug, Error)]
pub(crate) enum RateResolutionError {
    /// The configured policy could not resolve a rate.
    #[error(transparent)]
    Policy(#[from] DaFeeRatePolicyError),
    /// The policy query exceeded the configured timeout.
    #[error("DA fee-rate policy fetch timed out")]
    Timeout,
    /// The operator adjustment could not represent the final rate.
    #[error(transparent)]
    Adjustment(#[from] AffineAdjustmentError),
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
        let policy_rate = fetch_policy_rate(policy.as_ref()).await?;
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
        let adjusted_rate = adjustment.apply(policy_rate)?;
        let (updater, handle) = da_fee_rate_channel(adjusted_rate.wei_per_byte());

        Ok(Self {
            policy,
            adjustment,
            updater,
            handle,
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
        let adjusted_rate = self.adjustment.apply(rate)?;
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
        if !self.is_stale && now.duration_since(self.last_success_at) > self.stale_after {
            self.is_stale = true;
            true
        } else {
            false
        }
    }
}

/// Fetches a policy rate within the service's source timeout.
pub(super) async fn fetch_policy_rate(
    policy: &dyn DaFeeRatePolicy,
) -> Result<PolicyRate, RateResolutionError> {
    match timeout(POLICY_FETCH_TIMEOUT, policy.fetch_rate()).await {
        Ok(result) => result.map_err(RateResolutionError::Policy),
        Err(_) => Err(RateResolutionError::Timeout),
    }
}
