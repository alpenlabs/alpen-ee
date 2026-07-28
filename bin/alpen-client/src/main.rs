//! Reth node for the Alpen codebase.
//!
//! # Logging
//!
//! Alpen (non-reth) logs carry a `component = "alpen"` field so they can be
//! filtered apart from the embedded reth logs in monitoring. The field is
//! attached via `info_span!(..., component = "alpen")` spans, so it is only
//! present while those spans are enabled. Run this crate with the `alpen_client`
//! target at INFO or a more verbose level to get the tags: lowering it (e.g.
//! `RUST_LOG=alpen_client=warn`) or capping the compile-time level below info
//! (`tracing/release_max_level_*`) disables the spans and silently drops the tag.

mod args;
mod gossip;
mod node;
mod ol;
#[cfg(feature = "sequencer")]
mod sequencer;
mod service_executor;
mod services;

use std::{env, process, sync::Arc};

use alpen_chainspec::AlpenChainSpecParser;
use alpen_ee_params::AlpenSpecId;
use clap::Parser;
use reth_chainspec::ChainSpec;
use reth_cli_commands::{launcher::FnLauncher, node::NodeCommand};
use reth_cli_runner::{tokio_runtime, CliRunner};
use reth_cli_util::sigsegv_handler;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use strata_logging::{init_logging_from_config, LoggingInitConfig};
use tracing::{error, info};

use crate::args::{AdditionalConfig, ProverBackendConfig};

fn main() {
    sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: fine to set this in a non-async context.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }

    let mut command = NodeCommand::<AlpenChainSpecParser, AdditionalConfig>::parse();

    // use the EVM chain spec embedded in the Alpen params artifact
    // The boot spec pins v0; the fork-sensitive components (executor,
    // consensus, engine validator, payload builder) resolve the governing
    // version per block from the header-stamped spec version instead.
    // TODO(STR-3998): remaining version-blind consumers: pool tx validation
    // (tip policy), the p2p fork-id handshake, and the Alpen-layer check
    // that a block's claimed version matches the inbox-derived one.
    command.chain = command
        .ext
        .chain
        .alpen_params
        .chain_spec(AlpenSpecId::V0)
        .clone();
    // enable engine api v4
    command.engine.accept_execution_requests_hash = true;
    // allow chain fork blocks to be created
    command
        .engine
        .always_process_payload_attributes_on_canonical_head = true;

    if let Err(err) = run(command, node::launch) {
        eprintln!("Error: {err:?}");
        process::exit(1);
    }
}

/// Run node with logging
/// based on reth::cli::Cli::run
fn run<L>(
    command: NodeCommand<AlpenChainSpecParser, AdditionalConfig>,
    launcher: L,
) -> eyre::Result<()>
where
    L: std::ops::AsyncFnOnce(
        WithLaunchContext<NodeBuilder<Arc<reth_db::DatabaseEnv>, ChainSpec>>,
        AdditionalConfig,
    ) -> eyre::Result<()>,
{
    if command.ext.sequencer.enabled && !cfg!(feature = "sequencer") {
        error!(
            target: "alpen-client",
            component = "alpen",
            "Sequencer flag enabled but binary built without `sequencer` feature. Rebuild with default features or enable the `sequencer` feature."
        );
        eyre::bail!("sequencer feature not enabled at compile time");
    }

    if command.ext.sequencer.enabled {
        let prover_backend = command.ext.sequencer.prover.backend()?;
        if matches!(prover_backend, ProverBackendConfig::Sp1 { .. }) && !cfg!(feature = "sp1") {
            error!(
                target: "alpen-client",
                component = "alpen",
                "Remote SP1 prover requested but binary built without `sp1` feature. Pass --prover-backend native to use the native backend instead, or rebuild with the `sp1` feature."
            );
            eyre::bail!("sp1 feature not enabled at compile time");
        }
    }

    // Build the tokio runtime ourselves so logging init can run inside its
    // context, then hand it to CliRunner. The OTLP tracing exporter requires
    // an active tokio handle when it is built.
    let rt = tokio_runtime()?;

    {
        let _g = rt.handle().enter();

        let mut extra_filter_directives =
            vec!["sp1_core_executor=warn", "jsonrpsee_server::server=warn"];
        if let Some(verbosity_filter) = command.ext.display.verbosity_filter_directive() {
            extra_filter_directives.push(verbosity_filter);
        }

        init_logging_from_config(LoggingInitConfig {
            service_base_name: "alpen-client",
            service_label: command.ext.display.service_label.as_deref(),
            otlp_url: command.ext.display.otlp_url.as_deref(),
            log_dir: None,
            log_file_prefix: None,
            json_format: None,
            default_log_prefix: "alpen-client",
            extra_filter_directives: &extra_filter_directives,
        });
    }

    let runner = CliRunner::from_runtime(rt);

    info!(target: "alpen-client", component = "alpen", "logging initialized");

    let result = runner.run_command_until_exit(|ctx| {
        command.execute(
            ctx,
            FnLauncher::new::<AlpenChainSpecParser, AdditionalConfig>(launcher),
        )
    });

    // Flush OTLP tracing buffers before the process exits.
    strata_logging::finalize();

    result
}
