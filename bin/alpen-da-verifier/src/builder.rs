//! Assembles and launches the EE DA verifier service.

use std::sync::Arc;

use alpen_da_l1_extraction::{DaExtractor, FetchPolicy, FetchRetryPolicy};
use alpen_database::RecoveredDaDbOps;
use alpen_params::AlpenParams;
use bitcoind_async_client::Client;
use strata_common::retry::policies::ExponentialBackoff;
use strata_identifiers::L1Height;
use strata_service::{
    AsyncExecutor, DumbTickHandle, DumbTickingInput, ServiceBuilder, ServiceMonitor,
};
use tracing::info;

use crate::{
    bitcoin::ensure_bitcoin_network,
    config::DaVerifierConfig,
    context::DaVerifierContextImpl,
    ol_rpc::RpcOLAccountUpdateSource,
    service::{DaVerifierService, DaVerifierStatus},
    state::{DaRecoveryState, DaVerificationState, DaVerifierServiceState},
};

/// Owns the handles that keep the verifier worker and periodic input alive.
pub(crate) struct DaVerifierHandle {
    _monitor: ServiceMonitor<DaVerifierStatus>,
    _tick_handle: DumbTickHandle,
}

/// Owns the dependencies and configuration required to build the verifier service.
pub(crate) struct DaVerifierBuilder {
    params: AlpenParams,
    config: DaVerifierConfig,
    genesis_l1_height: L1Height,
    bitcoin_client: Client,
    recovered_da_db: RecoveredDaDbOps,
    account_update_source: RpcOLAccountUpdateSource,
}

impl DaVerifierBuilder {
    /// Creates an EE DA verifier service builder.
    pub(crate) fn new(
        params: AlpenParams,
        config: DaVerifierConfig,
        genesis_l1_height: L1Height,
        bitcoin_client: Client,
        recovered_da_db: RecoveredDaDbOps,
        account_update_source: RpcOLAccountUpdateSource,
    ) -> Self {
        Self {
            params,
            config,
            genesis_l1_height,
            bitcoin_client,
            recovered_da_db,
            account_update_source,
        }
    }

    /// Launches the verifier service on `executor`.
    ///
    /// # Errors
    ///
    /// Returns an error if Bitcoin network or EE account validation fails.
    pub(crate) async fn launch(
        self,
        executor: &impl AsyncExecutor,
    ) -> anyhow::Result<DaVerifierHandle> {
        self.check_bitcoin_network().await?;
        self.check_ee_account().await?;

        let l1_block_fetch_policy = self.build_l1_block_fetch_policy();
        // Read input configuration before transferring ownership into service state.
        let verification_interval = self.config.verification_interval();
        let service_state = self.into_service_state(l1_block_fetch_policy);

        let (tick_handle, tick_input) = DumbTickingInput::new(verification_interval);
        let monitor = ServiceBuilder::<DaVerifierService<DaVerifierContextImpl>, _>::new()
            .with_state(service_state)
            .with_input(tick_input)
            .launch_async("da_verifier", executor)
            .await?;

        Ok(DaVerifierHandle {
            _monitor: monitor,
            _tick_handle: tick_handle,
        })
    }

    async fn check_bitcoin_network(&self) -> anyhow::Result<()> {
        let bitcoin_network = self.config.bitcoin_network();
        ensure_bitcoin_network(&self.bitcoin_client, bitcoin_network).await?;
        info!(network = %bitcoin_network, "validated Bitcoin RPC network");
        Ok(())
    }

    async fn check_ee_account(&self) -> anyhow::Result<()> {
        let account_id = self.params.strata_exec_account_id();
        self.account_update_source
            .ensure_ee_account(account_id)
            .await?;
        info!(%account_id, "validated EE account against OL RPC");
        Ok(())
    }

    fn build_l1_block_fetch_policy(&self) -> FetchPolicy {
        let retry_policy = FetchRetryPolicy::new(
            self.config.fetch_max_retries(),
            ExponentialBackoff::new_with_default_multiplier(self.config.fetch_retry_delay_ms()),
        );

        FetchPolicy::new(retry_policy, self.config.block_fetch_concurrency())
    }

    fn into_service_state(
        self,
        l1_block_fetch_policy: FetchPolicy,
    ) -> DaVerifierServiceState<DaVerifierContextImpl> {
        let context = Arc::new(DaVerifierContextImpl::new(
            self.bitcoin_client,
            l1_block_fetch_policy,
            self.recovered_da_db,
            self.account_update_source,
        ));

        let da_extractor = DaExtractor::new(
            self.params.blob_spec().magic_bytes(),
            self.config.sequencer_pubkey(),
        );
        let recovery_state = DaRecoveryState::new(
            self.config.l1_reorg_safe_depth(),
            self.config.max_l1_scan_window_size(),
            da_extractor,
            self.genesis_l1_height,
        );
        let verification_state = DaVerificationState::new(self.params);

        DaVerifierServiceState::new(context, recovery_state, verification_state)
    }
}
