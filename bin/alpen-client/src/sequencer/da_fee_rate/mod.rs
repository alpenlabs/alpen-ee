//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A policy recommends a rate in wei per DA byte. The service applies
//! [`AffineAdjustment`] and publishes the result without exposing the selected
//! policy to payload construction.

mod service;
mod state;

use std::sync::Arc;

use alpen_reth_evm::WEI_PER_SAT;
use anyhow::Context;
use async_trait::async_trait;
use bitcoind_async_client::Client as BtcClient;
use strata_config::btcio::L1FeePolicyConfig;
use strata_service::AsyncExecutor;
use thiserror::Error;

pub(crate) use self::service::DaFeeRateServiceHandle;
#[cfg(test)]
use self::service::{DaFeeRateService, DaFeeRateStatus};
use self::state::*;
use super::bitcoin_fee_rate::resolve_fee_rate;
use crate::config::{DaFeeRateConfig, DaFeeRatePolicyConfig};

const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

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
    #[error("DA fee-rate source lookup failed: {0:#}")]
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
pub(crate) struct AffineAdjustment {
    multiplier_bps: u64,
    offset_wei_per_byte: u64,
}

impl AffineAdjustment {
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
    ) -> Result<AdjustedRate, AffineAdjustmentError> {
        let raw_rate = policy_rate.wei_per_byte() as u128;
        let adjusted = (raw_rate * self.multiplier_bps as u128).div_ceil(BASIS_POINTS_DENOMINATOR)
            + self.offset_wei_per_byte as u128;
        adjusted
            .try_into()
            .map(AdjustedRate)
            .map_err(|_| AffineAdjustmentError::Overflow(adjusted))
    }
}

impl Default for AffineAdjustment {
    fn default() -> Self {
        Self::new(10_000, 0)
    }
}

/// Reports that an adjusted DA fee rate cannot be represented as [`u64`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum AffineAdjustmentError {
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

/// Resolves the initial configured rate and starts its refresh service.
pub(crate) async fn start(
    config: &DaFeeRateConfig,
    btc_client: Arc<BtcClient>,
    writer_fee_policy_config: L1FeePolicyConfig,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<DaFeeRateServiceHandle> {
    let policy: Box<dyn DaFeeRatePolicy> = match config.policy() {
        DaFeeRatePolicyConfig::WriterBacked => Box::new(WriterBackedDaFeeRatePolicy::new(
            btc_client,
            writer_fee_policy_config,
        )),
        DaFeeRatePolicyConfig::Fixed { rate_wei_per_byte } => {
            Box::new(FixedDaFeeRatePolicy::new(rate_wei_per_byte))
        }
    };
    let state = DaFeeRateServiceState::initialize(policy, config)
        .await
        .context("failed to initialize DA fee rate")?;
    service::launch(state, executor).await
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{pending, Future},
        sync::Mutex,
        time::Duration,
    };

    use bitcoind_async_client::{corepc_types::bitcoin::FeeRate, Auth};
    use strata_config::btcio::{FeePolicy, MempoolExplorerFeePolicy};
    use strata_service::{AsyncGuard, AsyncService, Response, Service};
    use tokio::{
        task::{yield_now, JoinHandle},
        time::{advance, timeout},
    };

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

    struct NeverShutdown;

    impl AsyncGuard for NeverShutdown {
        async fn wait_for_shutdown(&self) {
            pending().await
        }
    }

    #[derive(Default)]
    struct TestExecutor {
        task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
    }

    impl TestExecutor {
        async fn join(&self) -> anyhow::Result<()> {
            let task = self
                .task
                .lock()
                .unwrap()
                .take()
                .expect("service task should have been spawned");
            task.await.expect("service task should not panic")
        }
    }

    impl AsyncExecutor for TestExecutor {
        type ShutdownGuard = NeverShutdown;

        fn spawn_async<F>(
            &self,
            _name: &'static str,
            worker: impl FnOnce(Self::ShutdownGuard) -> F + Send + 'static,
        ) where
            F: Future<Output = anyhow::Result<()>> + Send + 'static,
        {
            let previous = self
                .task
                .lock()
                .unwrap()
                .replace(tokio::spawn(worker(NeverShutdown)));
            assert!(previous.is_none(), "test executor only supports one task");
        }
    }

    fn rate_config(
        policy: DaFeeRatePolicyConfig,
        refresh_interval_seconds: u64,
        stale_after_seconds: u64,
        multiplier_bps: u64,
        offset_wei_per_byte: u64,
    ) -> DaFeeRateConfig {
        let policy_fields = match policy {
            DaFeeRatePolicyConfig::WriterBacked => "policy = \"writer_backed\"".to_owned(),
            DaFeeRatePolicyConfig::Fixed { rate_wei_per_byte } => {
                format!("policy = \"fixed\"\nfixed_rate_wei_per_byte = {rate_wei_per_byte}")
            }
        };
        toml::from_str(&format!(
            r#"
            {policy_fields}
            refresh_interval_seconds = {refresh_interval_seconds}
            stale_after_seconds = {stale_after_seconds}
            multiplier_bps = {multiplier_bps}
            offset_wei_per_byte = {offset_wei_per_byte}
            "#
        ))
        .expect("test DA fee-rate config should be valid")
    }

    fn service_config() -> DaFeeRateConfig {
        rate_config(DaFeeRatePolicyConfig::WriterBacked, 5, 10, 10_000, 0)
    }

    fn service_state_with_policy(
        policy: impl DaFeeRatePolicy,
        config: DaFeeRateConfig,
        initial_policy_rate: u64,
    ) -> DaFeeRateServiceState {
        DaFeeRateServiceState::new(
            Box::new(policy),
            &config,
            PolicyRate::new(initial_policy_rate),
        )
        .expect("test initial rate should be valid")
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
    ) -> Result<u64, AffineAdjustmentError> {
        AffineAdjustment::new(multiplier_bps, offset_wei_per_byte)
            .apply(PolicyRate::new(policy_rate))
            .map(AdjustedRate::wei_per_byte)
    }

    fn configured_rate(policy: DaFeeRatePolicyConfig) -> DaFeeRateConfig {
        rate_config(policy, 7, 21, 15_000, 3)
    }

    fn disconnected_bitcoin_client() -> Arc<BtcClient> {
        Arc::new(
            BtcClient::new(
                "http://127.0.0.1:1".to_string(),
                Auth::UserPass("test-user".to_string(), "test-password".to_string()),
                Some(1),
                Some(0),
                Some(1),
            )
            .expect("test Bitcoin client should be constructed"),
        )
    }

    #[test]
    fn default_adjustment_is_identity() {
        assert_eq!(
            AffineAdjustment::default()
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
            Err(AffineAdjustmentError::Overflow(_))
        ));
    }

    #[test]
    fn adjustment_rejects_offset_addition_that_exceeds_u64() {
        assert!(matches!(
            adjusted_rate(u64::MAX, 10_000, 1),
            Err(AffineAdjustmentError::Overflow(_))
        ));
    }

    #[tokio::test]
    async fn fixed_policy_returns_its_configured_rate() {
        let policy = FixedDaFeeRatePolicy::new(73);

        assert_eq!(policy.fetch_rate().await.unwrap().wei_per_byte(), 73);
    }

    #[tokio::test]
    async fn fixed_config_starts_with_adjusted_policy_rate() {
        let config = configured_rate(DaFeeRatePolicyConfig::Fixed {
            rate_wei_per_byte: 7,
        });
        let executor = TestExecutor::default();
        let service_handle = start(
            &config,
            disconnected_bitcoin_client(),
            L1FeePolicyConfig::new(FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(1),
            }),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(service_handle.rate_handle().current_rate(), 14);
        assert!(service_handle.stop());
        executor.join().await.unwrap();
    }

    #[tokio::test]
    async fn fixed_config_rejects_an_adjusted_rate_that_overflows() {
        let config = rate_config(
            DaFeeRatePolicyConfig::Fixed {
                rate_wei_per_byte: i64::MAX as u64,
            },
            5,
            10,
            20_001,
            0,
        );
        let executor = TestExecutor::default();

        let error = start(
            &config,
            disconnected_bitcoin_client(),
            L1FeePolicyConfig::new(FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(1),
            }),
            &executor,
        )
        .await
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("failed to initialize"), "{message}");
        assert!(message.contains("exceeds u64"), "{message}");
    }

    #[tokio::test]
    async fn writer_backed_config_reuses_writer_fee_policy() {
        let config = configured_rate(DaFeeRatePolicyConfig::WriterBacked);
        let executor = TestExecutor::default();
        let service_handle = start(
            &config,
            disconnected_bitcoin_client(),
            L1FeePolicyConfig::new(FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(125),
            }),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(service_handle.rate_handle().current_rate(), 1_875_000_003);
        assert!(service_handle.stop());
        executor.join().await.unwrap();
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
        const SECRET_URL: &str = "https://secret-user:secret-password@[::1";
        let policy = writer_policy(FeePolicy::MempoolExplorer {
            policy: MempoolExplorerFeePolicy::Fastest,
            mempool_base_url: SECRET_URL.to_string(),
            fallback_conf_target: 1,
        });

        let error = policy.fetch_rate().await.unwrap_err();

        assert!(matches!(&error, DaFeeRatePolicyError::Source(_)));
        let message = error.to_string();
        assert!(message.contains("invalid mempool_base_url"), "{message}");
        assert!(!message.contains(SECRET_URL), "{message}");
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
        let config = rate_config(DaFeeRatePolicyConfig::WriterBacked, 5, 10, 10_001, 0);
        let error = DaFeeRateServiceState::initialize(
            Box::new(ScriptedPolicy::new([Ok(PolicyRate::new(u64::MAX))])),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, RateResolutionError::Adjustment(_)));
    }

    #[test]
    fn successful_fetch_publishes_only_the_fully_adjusted_rate() {
        let config = rate_config(DaFeeRatePolicyConfig::WriterBacked, 5, 10, 15_000, 3);
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), config, 5);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(2);

        let update = state.apply_policy_rate(now, PolicyRate::new(7));

        assert!(matches!(
            update,
            Ok(RateUpdate {
                policy_rate: PolicyRate(7),
                adjusted_rate: AdjustedRate(14),
                changed: true,
                recovered: false,
            })
        ));
        assert_eq!(state.handle.current_rate(), 14);
        assert_eq!(state.last_success_at, now);
        assert!(!state.is_stale);
    }

    #[test]
    fn unchanged_success_still_refreshes_freshness() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = state.apply_policy_rate(now, PolicyRate::new(10));

        assert!(matches!(
            update,
            Ok(RateUpdate {
                changed: false,
                recovered: false,
                ..
            })
        ));
        assert_eq!(state.handle.current_rate(), 10);
        assert_eq!(state.last_success_at, now);
        assert!(!state.is_stale);
    }

    #[test]
    fn adjustment_failure_retains_the_current_rate_and_success_time() {
        let config = rate_config(DaFeeRatePolicyConfig::WriterBacked, 5, 10, 10_001, 0);
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), config, 1);
        let previous_success = state.last_success_at;
        let now = previous_success + Duration::from_secs(7);

        let update = state.apply_policy_rate(now, PolicyRate::new(u64::MAX));

        assert!(matches!(update, Err(RateResolutionError::Adjustment(_))));
        assert_eq!(state.handle.current_rate(), 2);
        assert_eq!(state.last_success_at, previous_success);
    }

    #[test]
    fn stale_boundary_is_strict_and_success_recovers() {
        let mut state = service_state_with_policy(ScriptedPolicy::new([]), service_config(), 10);
        let activated_at = state.last_success_at;

        assert!(!state.mark_stale_if_needed(activated_at + Duration::from_secs(10)));
        assert!(!state.is_stale);
        assert!(state.mark_stale_if_needed(
            activated_at + Duration::from_secs(10) + Duration::from_nanos(1)
        ));
        assert!(state.is_stale);

        let recovered_at = activated_at + Duration::from_secs(11);
        let update = state.apply_policy_rate(recovered_at, PolicyRate::new(10));

        assert!(matches!(
            update,
            Ok(RateUpdate {
                changed: false,
                recovered: true,
                ..
            })
        ));
        assert!(!state.is_stale);
        assert_eq!(state.last_success_at, recovered_at);
    }

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

    #[tokio::test]
    async fn service_tick_retains_last_successful_rate_on_policy_failure() {
        let mut state = DaFeeRateServiceState::initialize(
            Box::new(ScriptedPolicy::new([
                Ok(PolicyRate::new(10)),
                Err("unavailable"),
            ])),
            &service_config(),
        )
        .await
        .unwrap();

        let response = DaFeeRateService::process_input(&mut state, ())
            .await
            .unwrap();

        assert_eq!(response, Response::Continue);
        assert_eq!(state.handle.current_rate(), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn service_tick_marks_stale_after_failed_fetch() {
        let mut state = service_state_with_policy(
            ScriptedPolicy::new([Err("unavailable")]),
            service_config(),
            10,
        );
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

    #[tokio::test(start_paused = true)]
    async fn service_start_waits_one_interval_before_refreshing_and_stops_cleanly() {
        let state = service_state_with_policy(
            ScriptedPolicy::new([Ok(PolicyRate::new(23))]),
            service_config(),
            10,
        );
        let executor = TestExecutor::default();

        let service_handle = service::launch(state, &executor).await.unwrap();
        let rate_handle = service_handle.rate_handle();
        yield_now().await;
        assert_eq!(rate_handle.current_rate(), 10);

        advance(Duration::from_secs(5)).await;
        timeout(Duration::from_secs(1), async {
            while rate_handle.current_rate() != 23 {
                yield_now().await;
            }
        })
        .await
        .expect("service should refresh after one interval");

        assert!(service_handle.stop());
        executor.join().await.unwrap();
    }
}
