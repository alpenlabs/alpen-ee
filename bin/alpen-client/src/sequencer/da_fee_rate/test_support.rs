//! Shared fixtures for DA fee-rate unit tests.

use std::{
    collections::VecDeque,
    future::{pending, Future},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bitcoind_async_client::{Auth, Client as BtcClient};
use strata_service::{AsyncExecutor, AsyncGuard};
use tokio::task::JoinHandle;

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

pub(super) struct NeverShutdown;

impl AsyncGuard for NeverShutdown {
    async fn wait_for_shutdown(&self) {
        pending().await
    }
}

#[derive(Default)]
pub(super) struct TestExecutor {
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

impl TestExecutor {
    pub(super) async fn join(&self) -> anyhow::Result<()> {
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

pub(super) fn configured_rate(policy: DaFeeRatePolicyConfig) -> DaFeeRateConfig {
    rate_config(policy, 7, 21, 15_000, 3)
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

pub(super) fn disconnected_bitcoin_client() -> Arc<BtcClient> {
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
