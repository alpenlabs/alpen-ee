//! CLI argument definitions for the alpen-client binary.
//!
//! [`AdditionalConfig`] is the reth CLI extension type plugged into
//! `NodeCommand<AlpenChainSpecParser, AdditionalConfig>`. Three flags, three
//! concerns: reth's own `--config` (reth internals, untouched here),
//! `--alpen-params` (chain/protocol parameters), `--alpen-config` (this
//! node's own configuration, see [`crate::config`]).

use std::{env, fs, path::Path, sync::Arc};

use alpen_ee_params::AlpenParams;
use clap::ArgAction;
use eyre::Context;
#[cfg(feature = "sequencer")]
use strata_primitives::buf::Buf32;

use crate::config::AlpenClientConfig;

/// Alpen-specific CLI args extending the reth default CLI.
#[derive(Debug, clap::Parser)]
pub(crate) struct AdditionalConfig {
    #[command(flatten)]
    pub display: DisplayArgs,

    /// Path to the JSON-serialized Alpen params artifact.
    ///
    /// Single source of truth for the chain: EE account id, bridge params,
    /// DA stream identity, fork schedule, and the embedded EVM chain spec.
    #[arg(
        long,
        value_name = "PATH",
        required = true,
        value_parser = alpen_params_value_parser,
    )]
    pub alpen_params: Arc<AlpenParams>,

    /// Path to this node's own TOML configuration file.
    ///
    /// Not reth's `--config` (that's reth's own `reth.toml`, untouched) and
    /// not `--alpen-params` (that's the chain/protocol artifact above) —
    /// this is node-local config: OL connection, full-node vs. sequencer
    /// mode, and (in sequencer mode) sealing/proving/DA settings. See
    /// `bin/alpen-client/testdata/config.{full_node,sequencer}.toml` for
    /// annotated examples of the schema.
    #[arg(
        long,
        value_name = "PATH",
        required = true,
        value_parser = alpen_config_value_parser,
    )]
    pub alpen_config: Arc<AlpenClientConfig>,
}

/// Logging and telemetry args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Display")]
pub(crate) struct DisplayArgs {
    /// Set the minimum log level.
    ///
    /// -v      Errors
    /// -vv     Warnings
    /// -vvv    Info
    /// -vvvv   Debug
    /// -vvvvv  Traces (warning: very verbose!)
    #[arg(short, long, action = ArgAction::Count, global = true, verbatim_doc_comment)]
    pub verbosity: u8,

    /// Silence all log output.
    #[arg(long, alias = "silent", short = 'q', global = true)]
    pub quiet: bool,

    /// OTLP gRPC endpoint for the OpenTelemetry collector.
    ///
    /// When set, `strata-logging` builds a tracer provider against this
    /// endpoint. Metrics stay on Reth's native recorder and Prometheus
    /// endpoint; use Reth's `--metrics` flag for `/metrics`.
    /// Falls back to the standard `OTEL_EXPORTER_OTLP_ENDPOINT` env var
    /// when the flag isn't passed.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_url: Option<String>,

    /// Optional service label suffix appended to the OpenTelemetry
    /// `service.name` resource attribute (e.g. `prod`, `dev`,
    /// `staging-v2`). Mirrors `bin/strata`'s `--service-label`.
    #[arg(long)]
    pub service_label: Option<String>,
}

impl DisplayArgs {
    /// Returns an EnvFilter-compatible directive for CLI verbosity flags.
    pub(crate) fn verbosity_filter_directive(&self) -> Option<&'static str> {
        if self.quiet {
            return Some("off");
        }

        match self.verbosity {
            0 => None,
            1 => Some("error"),
            2 => Some("warn"),
            3 => Some("info"),
            4 => Some("debug"),
            _ => Some("trace"),
        }
    }
}

/// Reads `SEQUENCER_PRIVATE_KEY`, required when running with sequencer mode.
///
/// Read exactly once per startup in [`crate::node::launch`], before common
/// bootstrap performs any network or database work. The parsed key is then
/// passed on to everything that needs it.
#[cfg(feature = "sequencer")]
pub(crate) fn sequencer_privkey_from_env() -> eyre::Result<Buf32> {
    let privkey_str = env::var("SEQUENCER_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!("SEQUENCER_PRIVATE_KEY environment variable is required in sequencer mode")
    })?;

    privkey_str
        .parse::<Buf32>()
        .map_err(|e| eyre::eyre!("Failed to parse SEQUENCER_PRIVATE_KEY as hex: {e}"))
}

/// Reads `STRATA_SUBMIT_RPC_TOKEN`, required when `ol.submit_url` is set.
///
/// A secret, like `SEQUENCER_PRIVATE_KEY` — deliberately not a TOML config
/// field (see `crate::config::OlSource::Rpc`'s doc comment).
pub(crate) fn ol_submit_bearer_token_from_env() -> eyre::Result<String> {
    env::var("STRATA_SUBMIT_RPC_TOKEN").map_err(|_| {
        eyre::eyre!(
            "STRATA_SUBMIT_RPC_TOKEN environment variable is required when ol.submit_url is set"
        )
    })
}

/// Loads the Alpen params artifact from a JSON file.
///
/// Runs at CLI parse time so the embedded chain spec is available before the
/// node command is assembled.
fn alpen_params_value_parser(path: &str) -> eyre::Result<Arc<AlpenParams>> {
    let path = Path::new(path);
    let json = fs::read_to_string(path)
        .with_context(|| format!("failed to read Alpen params file {path:?}"))?;
    let params: AlpenParams = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse Alpen params file {path:?}"))?;
    Ok(Arc::new(params))
}

/// Loads this node's own config from a TOML file.
fn alpen_config_value_parser(path: &str) -> eyre::Result<Arc<AlpenClientConfig>> {
    let path = Path::new(path);
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read Alpen config file {path:?}"))?;
    let config = AlpenClientConfig::from_toml_str(&contents)
        .with_context(|| format!("failed to parse Alpen config file {path:?}"))?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use alpen_chainspec::AlpenChainSpecParser;
    use clap::{CommandFactory, Parser};
    use reth_cli_commands::node::NodeCommand;

    use super::*;
    use crate::config::NodeMode;

    fn base_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
        let params_fixture: &'static str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/alpen-params.json");
        let config_fixture: &'static str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/config.full_node.toml"
        );
        let mut argv = vec![
            "alpen-client",
            "--alpen-params",
            params_fixture,
            "--alpen-config",
            config_fixture,
        ];
        argv.extend_from_slice(args);
        argv
    }

    fn parse_additional_config(args: &[&str]) -> AdditionalConfig {
        AdditionalConfig::parse_from(base_argv(args))
    }

    /// Both file-backed flags load at CLI parse time.
    #[test]
    fn alpen_params_and_config_flags_load_the_artifacts() {
        let config = parse_additional_config(&[]);

        assert_eq!(config.alpen_params.genesis_block_info().blocknum(), 0);
        assert!(matches!(config.alpen_config.mode, NodeMode::FullNode(_)));
    }

    /// Catches arg id / flag collisions between the flattened Alpen arg
    /// groups and reth's own `NodeCommand` args (clap only surfaces these
    /// as debug asserts at command build time). Also guards against ever
    /// reintroducing a flag named `--config`, which reth's own `NodeCommand`
    /// already owns for `reth.toml`.
    #[test]
    fn node_command_args_do_not_conflict() {
        NodeCommand::<AlpenChainSpecParser, AdditionalConfig>::command().debug_assert();
    }
}
