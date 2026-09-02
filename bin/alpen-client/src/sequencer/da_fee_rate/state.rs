//! Mutable state and state transitions for the DA fee-rate service.

use std::{fmt, time::Duration};

use alpen_reth_node::{da_fee_rate_channel, DaFeeRateHandle, DaFeeRateUpdater};
use thiserror::Error;
use tokio::time::Instant;

use super::{
    AdjustedRate, DaFeeRatePolicy, DaFeeRatePolicyError, PolicyRate, RateAdjustment,
    RateAdjustmentError,
};

const POLICY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Holds the mutable state owned by [`super::service::DaFeeRateService`].
pub(crate) struct DaFeeRateServiceState {
    /// Resolves unadjusted rates from the configured source.
    pub(super) policy: Box<dyn DaFeeRatePolicy>,
    /// Applies the operator's multiplier and offset before publication.
    adjustment: RateAdjustment,
    /// Owns the capability to publish a new cached rate.
    updater: DaFeeRateUpdater,
    /// Provides read-only access to the currently published rate.
    pub(super) handle: DaFeeRateHandle,
    /// Controls how often the policy is queried.
    pub(super) refresh_interval: Duration,
    /// Marks the service stale after this long without a successful publication.
    pub(super) stale_after: Duration,
    /// Bounds one policy query so refreshes cannot hang indefinitely.
    pub(super) fetch_timeout: Duration,
    /// Tracks freshness from fallback activation or the latest successful publication.
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
            .field("fetch_timeout", &self.fetch_timeout)
            .field("last_success_at", &self.last_success_at)
            .field("is_stale", &self.is_stale)
            .finish_non_exhaustive()
    }
}

/// Runtime settings used to initialize [`DaFeeRateServiceState`].
#[derive(Clone, Copy, Debug)]
pub(super) struct DaFeeRateServiceConfig {
    /// Seeds the cache until the policy produces its first usable rate.
    fallback_policy_rate: PolicyRate,
    /// Applies to both the fallback and every fetched policy rate.
    adjustment: RateAdjustment,
    /// Controls how often the service queries its policy.
    refresh_interval: Duration,
    /// Defines how long the service may go without a successful publication.
    stale_after: Duration,
    /// Bounds the duration of one policy query.
    fetch_timeout: Duration,
}

impl DaFeeRateServiceConfig {
    /// Creates service settings with the built-in policy-fetch timeout.
    pub(super) const fn new(
        fallback_policy_rate: PolicyRate,
        adjustment: RateAdjustment,
        refresh_interval: Duration,
        stale_after: Duration,
    ) -> Self {
        Self {
            fallback_policy_rate,
            adjustment,
            refresh_interval,
            stale_after,
            fetch_timeout: POLICY_FETCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) const fn with_fetch_timeout(mut self, fetch_timeout: Duration) -> Self {
        self.fetch_timeout = fetch_timeout;
        self
    }
}

/// Reports invalid DA fee-rate service settings.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum DaFeeRateServiceError {
    #[error("DA fee-rate refresh interval must be nonzero")]
    ZeroRefreshInterval,
    #[error("DA fee-rate stale threshold must be at least the refresh interval")]
    StaleBeforeRefresh,
    #[error("DA fee-rate fetch timeout must be nonzero")]
    ZeroFetchTimeout,
    #[error("adjusted fallback DA fee rate is invalid")]
    InvalidFallback(#[source] RateAdjustmentError),
}

/// Reports why a refresh attempt did not publish a new rate.
#[derive(Debug, Error)]
pub(super) enum RefreshError {
    /// The configured policy could not resolve a rate.
    #[error(transparent)]
    Policy(#[from] DaFeeRatePolicyError),
    /// The policy query exceeded the configured timeout.
    #[error("DA fee-rate policy fetch timed out")]
    Timeout,
    /// The operator adjustment could not represent the final rate.
    #[error(transparent)]
    Adjustment(#[from] RateAdjustmentError),
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
    /// Initializes the payload handle with the adjusted fallback policy rate.
    pub(super) fn new(
        policy: Box<dyn DaFeeRatePolicy>,
        config: DaFeeRateServiceConfig,
    ) -> Result<Self, DaFeeRateServiceError> {
        if config.refresh_interval.is_zero() {
            return Err(DaFeeRateServiceError::ZeroRefreshInterval);
        }
        if config.stale_after < config.refresh_interval {
            return Err(DaFeeRateServiceError::StaleBeforeRefresh);
        }
        if config.fetch_timeout.is_zero() {
            return Err(DaFeeRateServiceError::ZeroFetchTimeout);
        }

        let fallback = config
            .adjustment
            .apply(config.fallback_policy_rate)
            .map_err(DaFeeRateServiceError::InvalidFallback)?;
        let (updater, handle) = da_fee_rate_channel(fallback.wei_per_byte());

        Ok(Self {
            policy,
            adjustment: config.adjustment,
            updater,
            handle,
            refresh_interval: config.refresh_interval,
            stale_after: config.stale_after,
            fetch_timeout: config.fetch_timeout,
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
    ) -> Result<RateUpdate, RefreshError> {
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
