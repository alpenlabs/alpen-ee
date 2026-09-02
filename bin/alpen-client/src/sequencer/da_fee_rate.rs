//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A policy recommends a rate in wei per DA byte. The controller applies
//! [`RateAdjustment`] and publishes the result without exposing the selected
//! policy to payload construction.

use std::{sync::Arc, time::Duration};

use alpen_reth_evm::WEI_PER_SAT;
use alpen_reth_node::{da_fee_rate_channel, DaFeeRateHandle, DaFeeRateUpdater};
use async_trait::async_trait;
use bitcoind_async_client::Client as BtcClient;
use strata_config::btcio::L1FeePolicyConfig;
use strata_service::{AsyncExecutor, AsyncGuard};
use thiserror::Error;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::bitcoin_fee_rate::resolve_fee_rate;

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;
const POLICY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A rate in wei per DA byte returned by a [`DaFeeRatePolicy`], before operator adjustment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PolicyRate(u64);

impl PolicyRate {
    /// Creates a policy rate denominated in wei per DA byte.
    pub(crate) const fn new(wei_per_byte: u64) -> Self {
        Self(wei_per_byte)
    }

    /// Returns the rate in wei per DA byte.
    pub(crate) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// A policy rate after applying the operator multiplier and offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdjustedRate(u64);

impl AdjustedRate {
    /// Returns the adjusted rate in wei per DA byte.
    pub(crate) const fn wei_per_byte(self) -> u64 {
        self.0
    }
}

/// Reports a failure to produce a usable policy rate.
#[derive(Debug, Error)]
pub(crate) enum DaFeeRatePolicyError {
    /// The selected external source did not return a rate.
    #[error("DA fee-rate source lookup failed")]
    Source(#[source] anyhow::Error),
    /// Converting a source-specific rate to wei per DA byte exceeded [`u64`].
    #[error("DA fee-rate conversion exceeds u64")]
    ConversionOverflow,
}

/// Supplies an unadjusted recommendation in wei per DA byte.
#[async_trait]
pub(crate) trait DaFeeRatePolicy: Send + Sync + 'static {
    /// Fetches the policy's current recommended rate.
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError>;
}

/// Applies the operator-configured multiplier and per-byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RateAdjustment {
    multiplier_bps: u64,
    offset_wei_per_byte: u64,
}

impl RateAdjustment {
    /// Creates an adjustment expressed in basis points and wei per DA byte.
    pub(crate) const fn new(multiplier_bps: u64, offset_wei_per_byte: u64) -> Self {
        Self {
            multiplier_bps,
            offset_wei_per_byte,
        }
    }

    /// Applies the multiplier with upward rounding, then adds the offset.
    pub(crate) fn apply(
        self,
        policy_rate: PolicyRate,
    ) -> Result<AdjustedRate, RateAdjustmentError> {
        let raw_rate = policy_rate.wei_per_byte() as u128;
        let adjusted = (raw_rate * self.multiplier_bps as u128).div_ceil(BASIS_POINTS_DENOMINATOR)
            + self.offset_wei_per_byte as u128;
        adjusted
            .try_into()
            .map(AdjustedRate)
            .map_err(|_| RateAdjustmentError::Overflow(adjusted))
    }
}

impl Default for RateAdjustment {
    fn default() -> Self {
        Self::new(10_000, 0)
    }
}

/// Reports that an adjusted DA fee rate cannot be represented as [`u64`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum RateAdjustmentError {
    #[error("adjusted DA fee rate exceeds u64: {0}")]
    Overflow(u128),
}

/// Returns one configured policy rate without consulting an external source.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixedDaFeeRatePolicy {
    rate: PolicyRate,
}

impl FixedDaFeeRatePolicy {
    /// Creates a fixed policy denominated in wei per DA byte.
    pub(crate) const fn new(rate_wei_per_byte: u64) -> Self {
        Self {
            rate: PolicyRate::new(rate_wei_per_byte),
        }
    }
}

#[async_trait]
impl DaFeeRatePolicy for FixedDaFeeRatePolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        Ok(self.rate)
    }
}

/// Resolves the Bitcoin writer's fee policy and converts it into DA pricing units.
pub(crate) struct WriterBackedDaFeeRatePolicy {
    client: Arc<BtcClient>,
    fee_policy_config: L1FeePolicyConfig,
}

impl WriterBackedDaFeeRatePolicy {
    /// Creates a policy backed by the configured Bitcoin fee-rate source.
    pub(crate) fn new(client: Arc<BtcClient>, fee_policy_config: L1FeePolicyConfig) -> Self {
        Self {
            client,
            fee_policy_config,
        }
    }
}

#[async_trait]
impl DaFeeRatePolicy for WriterBackedDaFeeRatePolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        let fee_rate = resolve_fee_rate(self.client.as_ref(), &self.fee_policy_config)
            .await
            .map_err(DaFeeRatePolicyError::Source)?;
        let fee_kwu = fee_rate.to_sat_per_kwu();
        const WEIGHT_UNITS_PER_KWU: u64 = 1000;
        let rate_per_da =
            (fee_kwu as u128 * WEI_PER_SAT as u128).div_ceil(WEIGHT_UNITS_PER_KWU as u128);
        let rate_per_da =
            u64::try_from(rate_per_da).map_err(|_| DaFeeRatePolicyError::ConversionOverflow)?;
        Ok(PolicyRate(rate_per_da))
    }
}

/// Controls policy refreshes and publication of the latest usable DA fee rate.
pub(crate) struct DaFeeRateController {
    /// Resolves unadjusted rates from the configured source.
    policy: Box<dyn DaFeeRatePolicy>,
    /// Applies the operator's multiplier and offset before publication.
    adjustment: RateAdjustment,
    /// Owns the capability to publish a new cached rate.
    updater: DaFeeRateUpdater,
    /// Provides read-only access to the currently published rate.
    handle: DaFeeRateHandle,
    /// Controls how often the policy is queried.
    refresh_interval: Duration,
    /// Marks the controller stale after this long without a successful publication.
    stale_after: Duration,
    /// Bounds one policy query so refreshes cannot hang indefinitely.
    fetch_timeout: Duration,
    /// Tracks freshness from fallback activation or the latest successful publication.
    last_success_at: Instant,
    /// Records whether the controller has crossed its stale threshold.
    is_stale: bool,
}

/// Runtime settings used by [`DaFeeRateController`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct DaFeeRateControllerConfig {
    /// Seeds the cache until the policy produces its first usable rate.
    fallback_policy_rate: PolicyRate,
    /// Applies to both the fallback and every fetched policy rate.
    adjustment: RateAdjustment,
    /// Controls how often the controller queries its policy.
    refresh_interval: Duration,
    /// Defines how long the controller may go without a successful publication.
    stale_after: Duration,
    /// Bounds the duration of one policy query.
    fetch_timeout: Duration,
}

impl DaFeeRateControllerConfig {
    /// Creates controller settings with the built-in policy-fetch timeout.
    pub(crate) const fn new(
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
    const fn with_fetch_timeout(mut self, fetch_timeout: Duration) -> Self {
        self.fetch_timeout = fetch_timeout;
        self
    }
}

/// Reports invalid controller startup settings.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum DaFeeRateControllerError {
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
enum ControllerRefreshError {
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
struct ControllerUpdate {
    /// Contains the rate returned directly by the policy.
    policy_rate: PolicyRate,
    /// Contains the final rate made visible to payload construction.
    adjusted_rate: AdjustedRate,
    /// Indicates whether publication changed the cached numeric value.
    changed: bool,
    /// Indicates whether this success transitioned the controller from stale to fresh.
    recovered: bool,
}

impl DaFeeRateController {
    /// Initializes the handle with the adjusted fallback policy rate.
    pub(crate) fn new(
        policy: Box<dyn DaFeeRatePolicy>,
        config: DaFeeRateControllerConfig,
    ) -> Result<Self, DaFeeRateControllerError> {
        if config.refresh_interval.is_zero() {
            return Err(DaFeeRateControllerError::ZeroRefreshInterval);
        }
        if config.stale_after < config.refresh_interval {
            return Err(DaFeeRateControllerError::StaleBeforeRefresh);
        }
        if config.fetch_timeout.is_zero() {
            return Err(DaFeeRateControllerError::ZeroFetchTimeout);
        }

        let fallback = config
            .adjustment
            .apply(config.fallback_policy_rate)
            .map_err(DaFeeRateControllerError::InvalidFallback)?;
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

    /// Starts the supervised refresh loop and returns its read-only rate handle.
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "called by sequencer integration in a later checkpoint"
        )
    )]
    pub(crate) fn start<E>(mut self, executor: &E) -> DaFeeRateHandle
    where
        E: AsyncExecutor,
    {
        self.last_success_at = Instant::now();
        let handle = self.handle.clone();
        info!(
            fallback_rate_wei_per_byte = handle.current_rate(),
            "DA fee-rate controller activated fallback"
        );
        executor.spawn_async("da_fee_rate_controller", move |shutdown| self.run(shutdown));
        handle
    }

    /// Refreshes the cached rate until the service receives a shutdown signal.
    async fn run<G>(mut self, shutdown: G) -> anyhow::Result<()>
    where
        G: AsyncGuard,
    {
        let mut refresh = interval(self.refresh_interval);
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.wait_for_shutdown() => return Ok(()),
                _ = refresh.tick() => {}
            }

            let fetch_result = tokio::select! {
                _ = shutdown.wait_for_shutdown() => return Ok(()),
                result = timeout(self.fetch_timeout, self.policy.fetch_rate()) => {
                    match result {
                        Ok(result) => result.map_err(ControllerRefreshError::Policy),
                        Err(_) => Err(ControllerRefreshError::Timeout),
                    }
                }
            };
            let now = Instant::now();
            match fetch_result.and_then(|rate| self.apply_policy_rate(now, rate)) {
                Ok(update) => self.log_update(&update),
                Err(error) => warn!(
                    %error,
                    current_rate_wei_per_byte = self.handle.current_rate(),
                    "DA fee-rate refresh failed; retaining the last usable rate"
                ),
            }

            if self.mark_stale_if_needed(now) {
                warn!(
                    current_rate_wei_per_byte = self.handle.current_rate(),
                    stale_after_seconds = self.stale_after.as_secs(),
                    "DA fee rate became stale; retaining the last usable rate"
                );
            }
        }
    }

    /// Adjusts and publishes a successfully fetched policy rate.
    ///
    /// Successful publication refreshes [`Self::last_success_at`] and clears
    /// [`Self::is_stale`]. The returned recovery flag preserves whether the
    /// controller was stale immediately before that transition.
    fn apply_policy_rate(
        &mut self,
        now: Instant,
        rate: PolicyRate,
    ) -> Result<ControllerUpdate, ControllerRefreshError> {
        let adjusted_rate = self.adjustment.apply(rate)?;
        let changed =
            self.updater.publish(adjusted_rate.wei_per_byte()) != adjusted_rate.wei_per_byte();

        let recovered = self.is_stale;
        self.is_stale = false;
        self.last_success_at = now;

        Ok(ControllerUpdate {
            policy_rate: rate,
            adjusted_rate,
            changed,
            recovered,
        })
    }

    /// Marks the controller stale and returns `true` only on the fresh-to-stale transition.
    fn mark_stale_if_needed(&mut self, now: Instant) -> bool {
        if !self.is_stale && now.duration_since(self.last_success_at) > self.stale_after {
            self.is_stale = true;
            true
        } else {
            false
        }
    }

    /// Records a successful rate publication and any state transition it caused.
    fn log_update(&self, update: &ControllerUpdate) {
        let ControllerUpdate {
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
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::pending, sync::Mutex};

    use bitcoind_async_client::{corepc_types::bitcoin::FeeRate, Auth};
    use strata_config::btcio::{FeePolicy, MempoolExplorerFeePolicy};
    use tokio::{sync::watch, task::yield_now, time::advance};

    use super::*;

    struct ScriptedPolicy {
        outcomes: Mutex<VecDeque<Result<PolicyRate, &'static str>>>,
    }

    impl ScriptedPolicy {
        fn new(outcomes: impl IntoIterator<Item = Result<PolicyRate, &'static str>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl DaFeeRatePolicy for ScriptedPolicy {
        async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("test policy should have another outcome")
                .map_err(|message| DaFeeRatePolicyError::Source(anyhow::anyhow!(message)))
        }
    }

    struct PendingPolicy;

    #[async_trait]
    impl DaFeeRatePolicy for PendingPolicy {
        async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
            pending().await
        }
    }

    struct TestShutdownGuard {
        shutdown_rx: watch::Receiver<bool>,
    }

    impl AsyncGuard for TestShutdownGuard {
        async fn wait_for_shutdown(&self) {
            let mut shutdown_rx = self.shutdown_rx.clone();
            if *shutdown_rx.borrow() {
                return;
            }
            while shutdown_rx.changed().await.is_ok() {
                if *shutdown_rx.borrow() {
                    return;
                }
            }
        }
    }

    fn controller_config(fallback_policy_rate: u64) -> DaFeeRateControllerConfig {
        DaFeeRateControllerConfig::new(
            PolicyRate::new(fallback_policy_rate),
            RateAdjustment::default(),
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
    }

    fn controller_with_policy(
        policy: impl DaFeeRatePolicy,
        config: DaFeeRateControllerConfig,
    ) -> DaFeeRateController {
        DaFeeRateController::new(Box::new(policy), config).unwrap()
    }

    fn writer_policy(fee_policy: FeePolicy) -> WriterBackedDaFeeRatePolicy {
        let client = BtcClient::new(
            "http://127.0.0.1:1".to_string(),
            Auth::UserPass("test-user".to_string(), "test-password".to_string()),
            Some(1),
            Some(0),
            Some(1),
        )
        .expect("test Bitcoin client should be constructed");
        WriterBackedDaFeeRatePolicy::new(Arc::new(client), L1FeePolicyConfig::new(fee_policy))
    }

    fn adjusted_rate(
        policy_rate: u64,
        multiplier_bps: u64,
        offset_wei_per_byte: u64,
    ) -> Result<u64, RateAdjustmentError> {
        RateAdjustment::new(multiplier_bps, offset_wei_per_byte)
            .apply(PolicyRate::new(policy_rate))
            .map(AdjustedRate::wei_per_byte)
    }

    #[test]
    fn default_adjustment_is_identity() {
        assert_eq!(
            RateAdjustment::default()
                .apply(PolicyRate::new(42))
                .unwrap()
                .wei_per_byte(),
            42
        );
    }

    #[test]
    fn adjustment_rounds_multiplier_up_before_adding_offset() {
        assert_eq!(adjusted_rate(5, 15_000, 3), Ok(11));
        assert_eq!(adjusted_rate(1, 10_001, 0), Ok(2));
    }

    #[test]
    fn zero_multiplier_leaves_only_offset() {
        assert_eq!(adjusted_rate(u64::MAX, 0, 17), Ok(17));
    }

    #[test]
    fn adjustment_rejects_scaled_rate_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_001, 0),
            Err(RateAdjustmentError::Overflow(_))
        ));
    }

    #[test]
    fn adjustment_rejects_offset_addition_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_000, 1),
            Err(RateAdjustmentError::Overflow(_))
        ));
    }

    #[tokio::test]
    async fn fixed_policy_returns_its_configured_rate() {
        let policy = FixedDaFeeRatePolicy::new(73);

        assert_eq!(policy.fetch_rate().await.unwrap().wei_per_byte(), 73);
    }

    #[test]
    fn policy_trait_is_object_safe() {
        fn accept_policy(_: &dyn DaFeeRatePolicy) {}

        accept_policy(&FixedDaFeeRatePolicy::new(1));
    }

    #[tokio::test]
    async fn writer_policy_converts_sat_per_kwu_to_wei_per_da_byte() {
        for (sat_per_kwu, expected_wei_per_byte) in [
            (1, 10_000_000),
            (125, 1_250_000_000),
            (1_000, 10_000_000_000),
        ] {
            let policy = writer_policy(FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(sat_per_kwu),
            });

            assert_eq!(
                policy.fetch_rate().await.unwrap().wei_per_byte(),
                expected_wei_per_byte
            );
        }
    }

    #[tokio::test]
    async fn writer_policy_maps_fee_source_failures() {
        let policy = writer_policy(FeePolicy::MempoolExplorer {
            policy: MempoolExplorerFeePolicy::Fastest,
            mempool_base_url: "not a url".to_string(),
            fallback_conf_target: 1,
        });

        assert!(matches!(
            policy.fetch_rate().await,
            Err(DaFeeRatePolicyError::Source(_))
        ));
    }

    #[tokio::test]
    async fn writer_policy_rejects_conversion_overflow() {
        let policy = writer_policy(FeePolicy::Fixed {
            fee_rate: FeeRate::from_sat_per_kwu(u64::MAX),
        });

        assert!(matches!(
            policy.fetch_rate().await,
            Err(DaFeeRatePolicyError::ConversionOverflow)
        ));
    }

    #[test]
    fn controller_starts_with_adjusted_fallback() {
        let config = DaFeeRateControllerConfig::new(
            PolicyRate::new(5),
            RateAdjustment::new(15_000, 3),
            Duration::from_secs(5),
            Duration::from_secs(10),
        );
        let controller = controller_with_policy(ScriptedPolicy::new([]), config);

        assert_eq!(controller.handle.current_rate(), 11);
    }

    #[test]
    fn controller_rejects_invalid_timing_and_fallback_settings() {
        let policy = || Box::new(ScriptedPolicy::new([])) as Box<dyn DaFeeRatePolicy>;

        let zero_refresh = DaFeeRateControllerConfig::new(
            PolicyRate::new(1),
            RateAdjustment::default(),
            Duration::ZERO,
            Duration::from_secs(1),
        );
        assert!(matches!(
            DaFeeRateController::new(policy(), zero_refresh),
            Err(DaFeeRateControllerError::ZeroRefreshInterval)
        ));

        let stale_before_refresh = DaFeeRateControllerConfig::new(
            PolicyRate::new(1),
            RateAdjustment::default(),
            Duration::from_secs(2),
            Duration::from_secs(1),
        );
        assert!(matches!(
            DaFeeRateController::new(policy(), stale_before_refresh),
            Err(DaFeeRateControllerError::StaleBeforeRefresh)
        ));

        let zero_timeout = controller_config(1).with_fetch_timeout(Duration::ZERO);
        assert!(matches!(
            DaFeeRateController::new(policy(), zero_timeout),
            Err(DaFeeRateControllerError::ZeroFetchTimeout)
        ));

        let overflowing_fallback = DaFeeRateControllerConfig::new(
            PolicyRate::new(u64::MAX),
            RateAdjustment::new(10_000, 1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(matches!(
            DaFeeRateController::new(policy(), overflowing_fallback),
            Err(DaFeeRateControllerError::InvalidFallback(_))
        ));
    }

    #[test]
    fn successful_fetch_publishes_only_the_fully_adjusted_rate() {
        let config = DaFeeRateControllerConfig::new(
            PolicyRate::new(5),
            RateAdjustment::new(15_000, 3),
            Duration::from_secs(5),
            Duration::from_secs(10),
        );
        let mut controller = controller_with_policy(ScriptedPolicy::new([]), config);
        let previous_success = controller.last_success_at;
        let now = previous_success + Duration::from_secs(2);

        let update = controller.apply_policy_rate(now, PolicyRate::new(7));

        assert!(matches!(
            update,
            Ok(ControllerUpdate {
                policy_rate: PolicyRate(7),
                adjusted_rate: AdjustedRate(14),
                changed: true,
                recovered: false,
            })
        ));
        assert_eq!(controller.handle.current_rate(), 14);
        assert_eq!(controller.last_success_at, now);
        assert!(!controller.is_stale);
    }

    #[test]
    fn unchanged_success_still_refreshes_freshness() {
        let mut controller = controller_with_policy(ScriptedPolicy::new([]), controller_config(10));
        let previous_success = controller.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = controller.apply_policy_rate(now, PolicyRate::new(10));

        assert!(matches!(
            update,
            Ok(ControllerUpdate {
                changed: false,
                recovered: false,
                ..
            })
        ));
        assert_eq!(controller.handle.current_rate(), 10);
        assert_eq!(controller.last_success_at, now);
        assert!(!controller.is_stale);
    }

    #[test]
    fn adjustment_failure_retains_the_current_rate_and_success_time() {
        let config = DaFeeRateControllerConfig::new(
            PolicyRate::new(1),
            RateAdjustment::new(10_001, 0),
            Duration::from_secs(5),
            Duration::from_secs(10),
        );
        let mut controller = controller_with_policy(ScriptedPolicy::new([]), config);
        let previous_success = controller.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = controller.apply_policy_rate(now, PolicyRate::new(u64::MAX));

        assert!(matches!(update, Err(ControllerRefreshError::Adjustment(_))));
        assert_eq!(controller.handle.current_rate(), 2);
        assert_eq!(controller.last_success_at, previous_success);
    }

    #[test]
    fn stale_boundary_is_strict_and_success_recovers() {
        let mut controller = controller_with_policy(ScriptedPolicy::new([]), controller_config(10));
        let activated_at = controller.last_success_at;

        assert!(!controller.mark_stale_if_needed(activated_at + Duration::from_secs(10)));
        assert!(!controller.is_stale);
        assert!(controller.mark_stale_if_needed(
            activated_at + Duration::from_secs(10) + Duration::from_nanos(1)
        ));
        assert!(controller.is_stale);

        let recovered_at = activated_at + Duration::from_secs(11);
        let update = controller.apply_policy_rate(recovered_at, PolicyRate::new(10));

        assert!(matches!(
            update,
            Ok(ControllerUpdate {
                changed: false,
                recovered: true,
                ..
            })
        ));
        assert!(!controller.is_stale);
        assert_eq!(controller.last_success_at, recovered_at);
    }

    #[tokio::test(start_paused = true)]
    async fn controller_loop_retains_fallback_on_policy_failure() {
        let controller = controller_with_policy(
            ScriptedPolicy::new([Err("unavailable")]),
            controller_config(10),
        );
        let handle = controller.handle.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(controller.run(TestShutdownGuard { shutdown_rx }));

        yield_now().await;
        assert_eq!(handle.current_rate(), 10);

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn controller_loop_fetches_immediately_and_stops_cleanly() {
        let controller = controller_with_policy(
            ScriptedPolicy::new([Ok(PolicyRate::new(23))]),
            controller_config(10),
        );
        let handle = controller.handle.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(controller.run(TestShutdownGuard { shutdown_rx }));

        yield_now().await;
        assert_eq!(handle.current_rate(), 23);

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn controller_loop_times_out_without_replacing_fallback() {
        let config = controller_config(10).with_fetch_timeout(Duration::from_secs(2));
        let controller = controller_with_policy(PendingPolicy, config);
        let handle = controller.handle.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(controller.run(TestShutdownGuard { shutdown_rx }));

        yield_now().await;
        advance(Duration::from_secs(2)).await;
        yield_now().await;
        assert_eq!(handle.current_rate(), 10);

        shutdown_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
