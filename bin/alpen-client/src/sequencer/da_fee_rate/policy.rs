//! Sources of unadjusted DA fee-rate recommendations.

use std::sync::Arc;

use alpen_reth_evm::WEI_PER_SAT;
use async_trait::async_trait;
use bitcoind_async_client::Client as BtcClient;
use strata_config::btcio::L1FeePolicyConfig;
use thiserror::Error;

use super::{
    super::bitcoin_fee_rate::{resolve_fee_rate, FeeRateResolutionTimeouts},
    rate::PolicyRate,
};

/// Reports a failure to produce a usable policy rate.
#[derive(Debug, Error)]
pub(super) enum DaFeeRatePolicyError {
    /// The selected external source did not return a rate.
    #[error("DA fee-rate source lookup failed: {0:#}")]
    Source(#[source] anyhow::Error),
    /// Converting a source-specific rate to wei per DA byte exceeded [`u64`].
    #[error("DA fee-rate conversion exceeds u64")]
    ConversionOverflow,
}

/// Supplies an unadjusted recommendation in wei per DA byte.
#[async_trait]
pub(super) trait DaFeeRatePolicy: Send + Sync + 'static {
    /// Fetches the policy's current recommended rate.
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError>;
}

/// Returns one configured policy rate without consulting an external source.
#[derive(Clone, Copy, Debug)]
pub(super) struct FixedDaFeeRatePolicy {
    rate: PolicyRate,
}

impl FixedDaFeeRatePolicy {
    /// Creates a fixed policy denominated in wei per DA byte.
    pub(super) const fn new(rate_wei_per_byte: u64) -> Self {
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
pub(super) struct WriterBackedDaFeeRatePolicy {
    client: Arc<BtcClient>,
    fee_policy_config: L1FeePolicyConfig,
    timeouts: FeeRateResolutionTimeouts,
}

impl WriterBackedDaFeeRatePolicy {
    /// Creates a policy backed by the configured Bitcoin fee-rate source.
    pub(super) fn new(
        client: Arc<BtcClient>,
        fee_policy_config: L1FeePolicyConfig,
        timeouts: FeeRateResolutionTimeouts,
    ) -> Self {
        Self {
            client,
            fee_policy_config,
            timeouts,
        }
    }
}

#[async_trait]
impl DaFeeRatePolicy for WriterBackedDaFeeRatePolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        let fee_rate =
            resolve_fee_rate(self.client.as_ref(), &self.fee_policy_config, self.timeouts)
                .await
                .map_err(DaFeeRatePolicyError::Source)?;
        let fee_kwu = fee_rate.to_sat_per_kwu();
        const WEIGHT_UNITS_PER_KWU: u64 = 1000;
        let rate_per_da =
            (fee_kwu as u128 * WEI_PER_SAT as u128).div_ceil(WEIGHT_UNITS_PER_KWU as u128);
        let rate_per_da =
            u64::try_from(rate_per_da).map_err(|_| DaFeeRatePolicyError::ConversionOverflow)?;
        Ok(PolicyRate::new(rate_per_da))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bitcoind_async_client::{corepc_types::bitcoin::FeeRate, Auth};
    use strata_config::btcio::{FeePolicy, MempoolExplorerFeePolicy};

    use super::*;

    fn writer_policy(fee_policy: FeePolicy) -> WriterBackedDaFeeRatePolicy {
        let client = BtcClient::new(
            "http://127.0.0.1:1".to_string(),
            Auth::UserPass("test-user".to_string(), "test-password".to_string()),
            Some(1),
            Some(0),
            Some(1),
        )
        .expect("test Bitcoin client should be constructed");
        WriterBackedDaFeeRatePolicy::new(
            Arc::new(client),
            L1FeePolicyConfig::new(fee_policy),
            FeeRateResolutionTimeouts::new(Duration::from_secs(10), Duration::from_secs(10)),
        )
    }

    #[tokio::test]
    async fn fixed_policy_returns_its_configured_rate() {
        let policy = FixedDaFeeRatePolicy::new(73);

        assert_eq!(policy.fetch_rate().await.unwrap().wei_per_byte(), 73);
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
}
