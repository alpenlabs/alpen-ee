//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A policy recommends a rate in wei per DA byte. The service applies
//! [`rate::AffineAdjustment`] and publishes the result without exposing the
//! selected policy to payload construction.

mod policy;
mod rate;
mod service;
mod state;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use anyhow::Context;
use bitcoind_async_client::Client as BtcClient;
use strata_config::btcio::L1FeePolicyConfig;
use strata_service::AsyncExecutor;

pub(crate) use self::service::DaFeeRateServiceHandle;
use self::{
    policy::{DaFeeRatePolicy, FixedDaFeeRatePolicy, WriterBackedDaFeeRatePolicy},
    state::DaFeeRateServiceState,
};
use super::bitcoin_fee_rate::FeeRateResolutionTimeouts;
use crate::config::{DaFeeRateConfig, DaFeeRatePolicyConfig};

/// Resolves the initial configured rate and starts its refresh service.
pub(crate) async fn start(
    config: &DaFeeRateConfig,
    btc_client: Arc<BtcClient>,
    writer_fee_policy_config: L1FeePolicyConfig,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<DaFeeRateServiceHandle> {
    let policy: Box<dyn DaFeeRatePolicy> = match config.policy() {
        DaFeeRatePolicyConfig::WriterBacked { .. } => Box::new(WriterBackedDaFeeRatePolicy::new(
            btc_client,
            writer_fee_policy_config,
            FeeRateResolutionTimeouts::new(config.explorer_timeout(), config.bitcoind_timeout()),
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
