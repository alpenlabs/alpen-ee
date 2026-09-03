//! Shared fixtures for DA fee-rate unit tests.

use std::{collections::VecDeque, future::pending, sync::Mutex};

use async_trait::async_trait;

use super::{
    policy::{DaFeeRatePolicy, DaFeeRatePolicyError},
    rate::PolicyRate,
    state::DaFeeRateServiceState,
};
use crate::config::{DaFeeRateConfig, DaFeeRatePolicyConfig};

pub(super) struct ScriptedPolicy {
    outcomes: Mutex<VecDeque<Result<PolicyRate, &'static str>>>,
}

impl ScriptedPolicy {
    pub(super) fn new(
        outcomes: impl IntoIterator<Item = Result<PolicyRate, &'static str>>,
    ) -> Self {
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

pub(super) struct PendingPolicy;

#[async_trait]
impl DaFeeRatePolicy for PendingPolicy {
    async fn fetch_rate(&self) -> Result<PolicyRate, DaFeeRatePolicyError> {
        pending().await
    }
}

pub(super) fn rate_config(
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

pub(super) fn service_config() -> DaFeeRateConfig {
    rate_config(DaFeeRatePolicyConfig::WriterBacked, 5, 10, 10_000, 0)
}

pub(super) fn service_state_with_policy(
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
