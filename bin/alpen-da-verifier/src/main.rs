//! Runs the Alpen EE DA verifier service.

mod account_state;
mod args;
mod bitcoin;
mod builder;
mod config;
mod context;
mod da_extraction;
mod evm_state;
mod ol_rpc;
mod service;
mod snapshot;
mod state;
#[cfg(test)]
mod tests;

use std::time::Duration;

use alpen_database::open_recovered_da_db;
use clap::Parser;
use strata_logging::{init_logging_from_config, LoggingInitConfigRef};
use strata_tasks::TaskManager;
use tokio::runtime::{Handle, Runtime};
use tracing::error;

use crate::{
    args::{load_alpen_params, load_bitcoind_rpc_credentials_from_env, Args},
    bitcoin::create_bitcoin_rpc_client,
    builder::DaVerifierBuilder,
    config::DaVerifierConfig,
    ol_rpc::RpcOLAccountUpdateSource,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let runtime = Runtime::new()?;
    let handle = runtime.handle().clone();
    let _runtime_guard = handle.enter();

    init_logging_from_config(LoggingInitConfigRef {
        service_base_name: "alpen-da-verifier",
        service_label: None,
        otlp_url: None,
        log_dir: None,
        log_file_prefix: None,
        json_format: None,
        default_log_prefix: "alpen-da-verifier",
        extra_filter_directives: &[],
    });

    let result = run(args, &handle);
    if let Err(error) = &result {
        error!(?error, "EE DA verifier exited with an error");
    }
    strata_logging::finalize();
    result
}

fn run(args: Args, handle: &Handle) -> anyhow::Result<()> {
    let params = load_alpen_params(&args.alpen_params)?;
    let config = DaVerifierConfig::from_path(&args.config)?;
    let bitcoind_credentials = load_bitcoind_rpc_credentials_from_env()?;

    let bitcoin_client = create_bitcoin_rpc_client(config.bitcoind(), bitcoind_credentials)?;
    let recovered_da_db = open_recovered_da_db(&args.datadir, handle.clone())
        .map_err(|error| anyhow::anyhow!("failed to open recovered EE DA database: {error}"))?;
    let account_update_source = RpcOLAccountUpdateSource::try_new(config.ol_rpc_url())
        .map_err(|error| anyhow::anyhow!("failed to create OL RPC client: {error}"))?;

    let task_manager = TaskManager::new(handle.clone());
    let executor = task_manager.create_executor();
    // Holding this handle keeps the periodic input alive until process shutdown.
    let _verifier_handle = handle.block_on(
        DaVerifierBuilder::new(
            params,
            config,
            args.genesis_l1_height,
            args.snapshot,
            bitcoin_client,
            recovered_da_db,
            account_update_source,
        )
        .launch(&executor),
    )?;

    task_manager.start_signal_listeners();
    task_manager.monitor(Some(SHUTDOWN_TIMEOUT))?;
    Ok(())
}
