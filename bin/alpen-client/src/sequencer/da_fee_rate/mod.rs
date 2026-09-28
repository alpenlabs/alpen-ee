//! Produces the DA fee rate used by sequencer payload builds.
//!
//! A fixed configuration publishes its final rate directly. A writer-backed
//! configuration periodically recommends a rate, applies [`rate::AffineAdjustment`],
//! and publishes the result without exposing the source to payload construction.

mod policy;
mod rate;
mod service;
mod state;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use alpen_reth_node::DaFeeRateHandle;
use anyhow::Context;
use bitcoind_async_client::Client as BtcClient;
use strata_btcio::writer::FeeRateResolutionTimeouts;
use strata_config::btcio::L1FeePolicyConfig;
use strata_service::AsyncExecutor;

use self::{
    policy::WriterBackedDaFeeRatePolicy, service::DaFeeRateServiceHandle,
    state::DaFeeRateServiceState,
};
use crate::config::DaFeeRateConfig;

/// Keeps the payload rate and any refresh service alive together.
#[derive(Debug)]
pub(crate) struct DaFeeRateRuntime {
    rate_handle: DaFeeRateHandle,
    _refresh_service: Option<DaFeeRateServiceHandle>,
}

impl DaFeeRateRuntime {
    /// Returns the non-blocking rate handle consumed by payload construction.
    pub(crate) fn rate_handle(&self) -> DaFeeRateHandle {
        self.rate_handle.clone()
    }
}

/// Creates a fixed rate or resolves and starts a writer-backed refresh service.
pub(crate) async fn start(
    config: &DaFeeRateConfig,
    btc_client: Arc<BtcClient>,
    writer_fee_policy_config: L1FeePolicyConfig,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<DaFeeRateRuntime> {
    match config {
        DaFeeRateConfig::Fixed { rate_wei_per_byte } => {
            let rate_handle = DaFeeRateHandle::fixed(*rate_wei_per_byte);
            service::record_initialized_rate(&rate_handle);
            Ok(DaFeeRateRuntime {
                rate_handle,
                _refresh_service: None,
            })
        }
        DaFeeRateConfig::WriterBacked { config } => {
            let policy = Box::new(WriterBackedDaFeeRatePolicy::new(
                btc_client,
                writer_fee_policy_config,
                FeeRateResolutionTimeouts::new(
                    config.explorer_timeout(),
                    config.bitcoind_timeout(),
                ),
            ));
            let state = DaFeeRateServiceState::initialize(policy, config)
                .await
                .context("failed to initialize DA fee rate")?;
            let refresh_service = service::launch(state, executor).await?;
            Ok(DaFeeRateRuntime {
                rate_handle: refresh_service.rate_handle(),
                _refresh_service: Some(refresh_service),
            })
        }
    }
}
