//! CLI argument definitions for the alpen-client binary.
//!
//! [`AdditionalConfig`] is the reth CLI extension type plugged into
//! `NodeCommand<AlpenChainSpecParser, AdditionalConfig>`. It is composed of
//! smaller [`clap::Args`] structs flattened into one command, grouped by
//! domain. Flattening does not namespace flags, so every `--long-flag` (and
//! env var) keeps its existing name; the grouping only affects code layout
//! and `--help` section headings.

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "sequencer")]
use alloy_primitives::{address, Address};
use alpen_ee_params::AlpenParams;
use clap::ArgAction;
use eyre::Context;
#[cfg(feature = "sequencer")]
use strata_config::btcio::{
    fee_rate_from_sat_per_vb, FeePolicy, L1FeePolicyConfig, MempoolExplorerFeePolicy, WriterConfig,
};
use strata_primitives::{buf::Buf32, L1Height};

const DEFAULT_HEALTH_CHECK_HOST: &str = "0.0.0.0";
const DEFAULT_HEALTH_CHECK_PORT: u16 = 8080;

// Mirrors `bitcoind-async-client`'s upstream defaults.
#[cfg(feature = "sequencer")]
const DEFAULT_BTCIO_RETRY_COUNT: u16 = 3;
#[cfg(feature = "sequencer")]
const DEFAULT_BTCIO_RETRY_INTERVAL_MS: u64 = 1_000;

#[cfg(feature = "sequencer")]
const DEFAULT_BENEFICIARY_ADDRESS: Address = address!("5400000000000000000000000000000000000010");

// Mirrors `alpen_ee_sequencer::BlockBuilderConfig`'s default target blocktime.
#[cfg(feature = "sequencer")]
const DEFAULT_BLOCKTIME_MS: u64 = 5_000;

/// Alpen-specific CLI args extending the reth default CLI.
///
/// Composed of domain-grouped [`clap::Args`] structs; all flags share one
/// flat namespace with reth's own args.
#[derive(Debug, clap::Parser)]
pub(crate) struct AdditionalConfig {
    #[command(flatten)]
    pub display: DisplayArgs,

    #[command(flatten)]
    pub chain: ChainArgs,

    #[command(flatten)]
    pub node: NodeArgs,

    #[command(flatten)]
    pub ol: OlArgs,

    #[command(flatten)]
    pub sequencer: SequencerArgs,

    #[command(flatten)]
    pub da: DaArgs,

    #[cfg(feature = "sequencer")]
    #[command(flatten)]
    pub btcio: BtcioArgs,
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

/// Chain spec and EE params args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Alpen Chain")]
pub(crate) struct ChainArgs {
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
}

/// Node-local service args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Alpen Node")]
pub(crate) struct NodeArgs {
    /// Host for the HTTP health check endpoint.
    #[arg(long, default_value = DEFAULT_HEALTH_CHECK_HOST)]
    pub health_check_host: String,

    /// Port for the HTTP health check endpoint.
    #[arg(long, default_value_t = DEFAULT_HEALTH_CHECK_PORT)]
    pub health_check_port: u16,

    #[arg(long, required = false)]
    pub db_retry_count: Option<u16>,
}

/// OL node connection args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "OL Connection")]
pub(crate) struct OlArgs {
    /// URL of OL node RPC (can be either `http[s]://` or `ws[s]://`).
    /// Required unless `--dummy-ol-client` is specified.
    #[arg(long = "ol-client-url", required_unless_present = "dummy_client")]
    pub client_url: Option<String>,

    /// URL of the authenticated OL transaction submission RPC.
    /// Required with `--sequencer` unless `--dummy-ol-client` is specified.
    #[arg(long = "ol-submit-url")]
    pub submit_url: Option<String>,

    /// Bearer token for the authenticated OL transaction submission RPC.
    #[arg(long = "ol-submit-bearer-token", env = "STRATA_SUBMIT_RPC_TOKEN")]
    pub submit_bearer_token: Option<String>,

    /// Use a dummy OL client instead of connecting to a real OL node.
    /// This is useful for testing EE functionality in isolation.
    ///
    /// NOTE: This is intentionally separate from OL-EE integration tests which
    /// need the real OL RPC client. The dummy client is only for EE-specific
    /// tests that don't need OL interaction.
    #[arg(long = "dummy-ol-client", default_value_t = false)]
    pub dummy_client: bool,

    /// Have the OL chain tracker advance against the latest completed OL
    /// epoch in the connected Strata node instead of the canonical
    /// `confirmed` epoch (CSM-based). Dev/test only. Useful when the CSM
    /// checkpoint pipeline can't keep up with rapid SAU emission and would
    /// otherwise stall the EE block builder's inbox-message fetch.
    #[arg(long, default_value_t = false)]
    pub dev_track_latest_epoch: bool,
}

/// Sequencer mode, block building, and proving args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Sequencer")]
pub(crate) struct SequencerArgs {
    /// Run the node as a sequencer. Requires the `sequencer` feature,
    /// a `SEQUENCER_PRIVATE_KEY` environment variable, and all DA-related
    /// arguments (`--btc-rpc-url`, `--btc-rpc-user`, `--btc-rpc-password`).
    #[arg(
        long = "sequencer",
        default_value_t = false,
        requires_all = ["btc_rpc_url", "btc_rpc_user", "btc_rpc_password"],
    )]
    pub enabled: bool,

    /// Sequencer's public key (hex-encoded, 32 bytes) for signature validation.
    #[arg(long = "sequencer-pubkey", required = true, value_parser = parse_buf32)]
    pub pubkey: Buf32,

    /// Rpc of sequencer's reth node to forward transactions to.
    #[arg(long = "sequencer-http", required = false)]
    pub http_url: Option<String>,

    /// Number of blocks per batch before sealing.
    /// Lower values seal batches more frequently (useful for testing).
    #[arg(long, default_value = "100")]
    pub batch_sealing_block_count: u64,

    /// Number of blocks per chunk before sealing.
    /// Defaults to `batch_sealing_block_count` if not set.
    #[arg(long, required = false)]
    pub chunk_sealing_block_count: Option<u64>,

    /// Cumulative gas limit per chunk before sealing.
    /// When set, a chunk is sealed when either the block count or the gas
    /// limit is reached (whichever comes first). When omitted, only the
    /// block count policy is used.
    #[arg(long, required = false)]
    pub chunk_sealing_gas_limit: Option<u64>,

    /// Capacity of the batch builder → chunk builder event channel.
    /// Defaults to 64 if not set.
    #[arg(long, required = false)]
    pub batch_event_channel_capacity: Option<usize>,

    #[command(flatten)]
    pub prover: ProverArgs,

    #[cfg(feature = "sequencer")]
    #[arg(long, default_value_t = DEFAULT_BENEFICIARY_ADDRESS)]
    pub beneficiary_address: Address,

    /// EE block time override, in milliseconds. Must be greater than zero.
    #[cfg(feature = "sequencer")]
    #[arg(
        long = "ee-block-time-ms",
        env = "ALPEN_EE_BLOCK_TIME_MS",
        default_value_t = DEFAULT_BLOCKTIME_MS,
        value_parser = clap::value_parser!(u64).range(1..),
    )]
    pub blocktime_ms: u64,
}

/// The chunk + acct path pair (ELF paths for `sp1`, signing-key file paths
/// for `native`). Coupled into a single CLI token so the two can't be
/// mismatched by passing them as separately-ordered flags.
///
/// Named around "program," not "version": this pair doesn't declare itself
/// active — it's a candidate the process validates against whatever VK the
/// OL currently expects (see `sequencer::prover::backend`). "Version"
/// reads as a claim of being *the* active one, which would be actively
/// misleading now that `--prover-program` is repeatable and can hold
/// several resident candidates at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProverProgramPaths {
    pub chunk_path: PathBuf,
    pub acct_path: PathBuf,
}

/// Parses a `--prover-program` value of the form `<chunk_path>:<acct_path>`.
fn parse_prover_program_paths(s: &str) -> eyre::Result<ProverProgramPaths> {
    let (chunk, acct) = s
        .split_once(':')
        .ok_or_else(|| eyre::eyre!("expected <chunk_path>:<acct_path>, got {s:?}"))?;
    if chunk.is_empty() || acct.is_empty() {
        return Err(eyre::eyre!("expected <chunk_path>:<acct_path>, got {s:?}"));
    }
    Ok(ProverProgramPaths {
        chunk_path: chunk.into(),
        acct_path: acct.into(),
    })
}

/// EE chunk/acct prover backend selection and configuration.
///
/// The raw flags below are cross-validated and resolved into a single
/// [`ProverBackendConfig`] by [`ProverArgs::backend`]. They're only
/// meaningful with `--sequencer`; a fullnode never touches the prover, so
/// nothing here is required at the clap level — `--prover-backend` and the
/// program it needs are validated together in [`ProverArgs::backend`]
/// instead, which only runs behind the `--sequencer` gate.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Prover")]
pub(crate) struct ProverArgs {
    /// EE chunk/acct prover backend.
    ///
    /// `native` (the default) reads signing-key file paths from
    /// `--prover-program`; `sp1` reads ELF paths the same way.
    #[arg(long = "prover-backend", value_enum, default_value_t = ProverBackendArg::Native)]
    pub backend: ProverBackendArg,

    /// End-to-end deadline (seconds) passed to the SP1 prover network on
    /// every chunk/acct proof request. Only used with the remote SP1
    /// backend. When unset, a built-in default is applied (see
    /// `DEFAULT_SP1_DEADLINE_SECS`).
    #[arg(long, required = false)]
    pub sp1_proof_deadline_secs: Option<u64>,

    /// The chunk+acct program, as `<chunk_path>:<acct_path>`. ELF paths
    /// under `--prover-backend sp1`, signing-key file paths under `native`.
    /// Repeatable: each occurrence adds a candidate program, and the one
    /// whose derived account predicate key matches the OL's expected
    /// `update_vk` at startup is the one actually used (see
    /// `sequencer::prover::backend`). At least one is required with
    /// `--sequencer`.
    #[arg(long = "prover-program", value_parser = parse_prover_program_paths, required = false)]
    pub prover_program: Vec<ProverProgramPaths>,
}

impl ProverArgs {
    /// Resolves the CLI flags into the EE chunk/acct prover backend.
    ///
    /// Fails if no `--prover-program` was given. Called both as an early
    /// fail-fast check in `main.rs` and again where the backend is actually
    /// built, so a bad flag combination is rejected before any node/DA/
    /// prover startup work begins rather than deep inside it.
    ///
    /// Only checks that at least one program is present, not that its paths
    /// exist or parse — reading them is left to the caller that actually
    /// builds the backend.
    pub(crate) fn backend(&self) -> eyre::Result<ProverBackendConfig> {
        if self.prover_program.is_empty() {
            return Err(eyre::eyre!("--prover-program is required with --sequencer"));
        }
        let programs = self.prover_program.clone();

        Ok(match self.backend {
            ProverBackendArg::Native => ProverBackendConfig::Native { programs },
            ProverBackendArg::Sp1 => ProverBackendConfig::Sp1 {
                programs,
                deadline_secs: self.sp1_proof_deadline_secs,
            },
        })
    }
}

/// CLI selector for [`ProverBackendConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ProverBackendArg {
    #[value(name = "native")]
    Native,
    #[value(name = "sp1")]
    Sp1,
}

/// EE chunk/acct prover backend, resolved from [`ProverArgs`].
///
/// A single tagged value instead of separately-validated paths, so the rest
/// of the sequencer startup path threads one already-validated value
/// instead of re-deriving the same cross-field requirement at every layer.
/// Holds paths rather than read file contents: reading them is left to
/// whoever actually builds the backend.
///
/// `programs` holds one or more candidates (see [`ProverProgramPaths`]);
/// the backend builder picks whichever one's derived account predicate key
/// matches the OL's expected `update_vk`.
#[derive(Debug, Clone)]
pub(crate) enum ProverBackendConfig {
    /// zkaleido `NativeHost`s, signing chunk/acct proofs with the keys read
    /// from the given paths.
    Native { programs: Vec<ProverProgramPaths> },
    /// SP1 remote hosts.
    Sp1 {
        programs: Vec<ProverProgramPaths>,
        /// Falls back to `DEFAULT_SP1_DEADLINE_SECS` when unset.
        deadline_secs: Option<u64>,
    },
}

/// EE DA and Bitcoin RPC args.
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "EE DA")]
pub(crate) struct DaArgs {
    /// Bitcoin Core RPC URL. Required when `--sequencer` is set.
    #[arg(long, required = false)]
    pub btc_rpc_url: Option<String>,

    /// Bitcoin Core RPC username. Required when `--sequencer` is set.
    #[arg(long, required = false)]
    pub btc_rpc_user: Option<String>,

    /// Bitcoin Core RPC password. Required when `--sequencer` is set.
    #[arg(long, required = false)]
    pub btc_rpc_password: Option<String>,

    /// L1 reorg safe depth (number of confirmations for finality).
    #[arg(long, default_value = "6")]
    pub l1_reorg_safe_depth: u32,

    /// Genesis L1 block height (the first L1 block the rollup cares about).
    #[arg(long, default_value = "0")]
    pub genesis_l1_height: L1Height,
}

/// btcio writer (fee policy) and Bitcoin RPC retry args.
#[cfg(feature = "sequencer")]
#[derive(Debug, clap::Args)]
#[command(next_help_heading = "Btcio Writer")]
pub(crate) struct BtcioArgs {
    /// btcio writer fee policy: `bitcoind`, `fixed`, or `mempool`.
    #[arg(long = "btcio-fee-policy", value_enum, default_value_t = BtcioFeePolicyArg::Bitcoind)]
    pub fee_policy: BtcioFeePolicyArg,

    /// Confirmation target for `bitcoind`; also the mempool fallback.
    #[arg(long = "btcio-conf-target", default_value = "1")]
    pub conf_target: u16,

    /// Fixed fee rate in sat/vB. Required when policy is `fixed`.
    #[arg(long = "btcio-fee-rate", required_if_eq("fee_policy", "fixed"))]
    pub fee_rate: Option<f64>,

    /// mempool.space-compatible base URL. Required when policy is `mempool`.
    #[arg(long = "btcio-mempool-base-url")]
    pub mempool_base_url: Option<String>,

    /// Mempool fee tier when policy is `mempool`.
    #[arg(long = "btcio-mempool-tier", value_enum, default_value_t = BtcioMempoolTierArg::Fastest)]
    pub mempool_tier: BtcioMempoolTierArg,

    /// Max retries for Bitcoin RPC requests.
    #[arg(long = "btcio-retry-count", default_value_t = DEFAULT_BTCIO_RETRY_COUNT)]
    pub retry_count: u16,

    /// Bitcoin RPC retry interval in ms.
    #[arg(long = "btcio-retry-interval", default_value_t = DEFAULT_BTCIO_RETRY_INTERVAL_MS)]
    pub retry_interval: u64,
}

#[cfg(feature = "sequencer")]
impl BtcioArgs {
    /// Builds [`WriterConfig`] from CLI flags. Empty-string mempool URL is
    /// treated as absent so docker-compose `${VAR:-}` doesn't yield `Some("")`.
    pub(crate) fn writer_config(&self) -> eyre::Result<WriterConfig> {
        let mempool_base_url = self
            .mempool_base_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let fee_policy = match self.fee_policy {
            BtcioFeePolicyArg::Bitcoind => FeePolicy::BitcoinD {
                conf_target: self.conf_target,
            },
            BtcioFeePolicyArg::Fixed => {
                let fee_rate_sat_per_vb = self.fee_rate.ok_or_else(|| {
                    eyre::eyre!("--btcio-fee-rate is required when --btcio-fee-policy=fixed")
                })?;
                let fee_rate = fee_rate_from_sat_per_vb(fee_rate_sat_per_vb)
                    .map_err(|err| eyre::eyre!("invalid --btcio-fee-rate: {err}"))?;
                FeePolicy::Fixed { fee_rate }
            }
            BtcioFeePolicyArg::Mempool => {
                let base_url = mempool_base_url.clone().ok_or_else(|| {
                    eyre::eyre!(
                        "--btcio-mempool-base-url is required when --btcio-fee-policy=mempool"
                    )
                })?;
                FeePolicy::MempoolExplorer {
                    policy: self.mempool_tier.into(),
                    mempool_base_url: base_url,
                    fallback_conf_target: self.conf_target,
                }
            }
        };
        Ok(WriterConfig {
            l1_fee_policy_config: L1FeePolicyConfig::new(fee_policy),
            ..WriterConfig::default()
        })
    }
}

/// CLI mirror of [`FeePolicy`].
#[cfg(feature = "sequencer")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BtcioFeePolicyArg {
    Bitcoind,
    Fixed,
    Mempool,
}

/// CLI mirror of [`MempoolExplorerFeePolicy`].
#[cfg(feature = "sequencer")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BtcioMempoolTierArg {
    Fastest,
    HalfHour,
    Hour,
    Economy,
    Minimum,
}

#[cfg(feature = "sequencer")]
impl From<BtcioMempoolTierArg> for MempoolExplorerFeePolicy {
    fn from(value: BtcioMempoolTierArg) -> Self {
        match value {
            BtcioMempoolTierArg::Fastest => Self::Fastest,
            BtcioMempoolTierArg::HalfHour => Self::HalfHour,
            BtcioMempoolTierArg::Hour => Self::Hour,
            BtcioMempoolTierArg::Economy => Self::Economy,
            BtcioMempoolTierArg::Minimum => Self::Minimum,
        }
    }
}

/// Parse a hex-encoded string into a [`Buf32`].
fn parse_buf32(s: &str) -> eyre::Result<Buf32> {
    s.parse::<Buf32>()
        .map_err(|e| eyre::eyre!("Failed to parse hex string as Buf32: {e}"))
}

/// Reads `SEQUENCER_PRIVATE_KEY`, required when running with `--sequencer`.
///
/// Called unconditionally at startup regardless of whether the `sequencer`
/// feature is compiled in: gossip block signing needs the raw key too, not
/// just the sequencer-only DA reveal path, so this can't live behind the
/// `sequencer` module boundary.
pub(crate) fn sequencer_privkey_from_env(sequencer_enabled: bool) -> eyre::Result<Option<Buf32>> {
    if !sequencer_enabled {
        return Ok(None);
    }

    let privkey_str = env::var("SEQUENCER_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "SEQUENCER_PRIVATE_KEY environment variable is required when running with --sequencer"
        )
    })?;

    let privkey = privkey_str
        .parse::<Buf32>()
        .map_err(|e| eyre::eyre!("Failed to parse SEQUENCER_PRIVATE_KEY as hex: {e}"))?;

    Ok(Some(privkey))
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

#[cfg(test)]
mod additional_config_tests {
    use alpen_chainspec::AlpenChainSpecParser;
    use clap::CommandFactory;
    use reth_cli_commands::node::NodeCommand;

    use super::*;

    const SEQUENCER_PUBKEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    fn base_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
        let params_fixture: &'static str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/alpen-params.json");
        let mut argv = vec![
            "alpen-client",
            "--alpen-params",
            params_fixture,
            "--sequencer-pubkey",
            SEQUENCER_PUBKEY,
        ];
        argv.extend_from_slice(args);
        argv
    }

    fn parse_additional_config(args: &[&str]) -> AdditionalConfig {
        <AdditionalConfig as clap::Parser>::parse_from(base_argv(args))
    }

    /// Like [`parse_additional_config`], but surfaces parse errors instead of
    /// exiting the process, for asserting on clap-level rejections.
    fn try_parse_additional_config(args: &[&str]) -> Result<AdditionalConfig, clap::Error> {
        <AdditionalConfig as clap::Parser>::try_parse_from(base_argv(args))
    }

    /// The artifact loads at CLI parse time and the genesis facts are
    /// derived from its embedded EVM spec.
    #[test]
    fn alpen_params_flag_loads_the_artifact() {
        let config = parse_additional_config(&["--dummy-ol-client"]);

        assert_eq!(config.chain.alpen_params.genesis_block_info().blocknum(), 0);
    }

    /// `--ol-client-url` is required unless `--dummy-ol-client` is set; this
    /// must be rejected at parse time, not deep inside `node::launch` after
    /// the database has already been opened.
    #[test]
    fn ol_client_url_required_unless_dummy_client() {
        let err = try_parse_additional_config(&[]).unwrap_err();
        assert!(err.to_string().contains("--ol-client-url"));
    }

    #[test]
    fn ol_client_url_not_required_with_dummy_client() {
        let config = try_parse_additional_config(&["--dummy-ol-client"]).unwrap();
        assert!(config.ol.client_url.is_none());
    }

    /// `--btcio-fee-rate` is required when `--btcio-fee-policy=fixed`; this
    /// must be rejected at parse time rather than inside `writer_config()`.
    #[cfg(feature = "sequencer")]
    #[test]
    fn btcio_fee_rate_required_when_policy_fixed() {
        let err =
            try_parse_additional_config(&["--dummy-ol-client", "--btcio-fee-policy", "fixed"])
                .unwrap_err();
        assert!(err.to_string().contains("--btcio-fee-rate"));
    }

    /// `--ee-block-time-ms` (aliasing `ALPEN_EE_BLOCK_TIME_MS`) must be
    /// greater than zero, enforced by clap's own range check.
    #[cfg(feature = "sequencer")]
    #[test]
    fn ee_block_time_ms_rejects_zero() {
        let err = try_parse_additional_config(&["--dummy-ol-client", "--ee-block-time-ms", "0"])
            .unwrap_err();
        assert!(err.to_string().contains("ee-block-time-ms"));
    }

    /// Catches arg id / flag collisions between the flattened Alpen arg
    /// groups and reth's own `NodeCommand` args (clap only surfaces these
    /// as debug asserts at command build time).
    #[test]
    fn node_command_args_do_not_conflict() {
        NodeCommand::<AlpenChainSpecParser, AdditionalConfig>::command().debug_assert();
    }

    /// Locks the CLI flag surface: docker entrypoints, compose files, and
    /// the functional-test factories all pass these long flags, so a field
    /// rename that forgets to pin the old name via `#[arg(long = "...")]`
    /// must fail here.
    #[cfg(feature = "sequencer")]
    #[test]
    fn legacy_flag_names_still_parse() {
        let config = parse_additional_config(&[
            "--verbosity",
            "--quiet",
            "--otlp-url",
            "http://localhost:4317",
            "--service-label",
            "test",
            "--sequencer-http",
            "http://localhost:8545",
            "--ol-client-url",
            "ws://localhost:8432",
            "--ol-submit-url",
            "ws://localhost:8435",
            "--ol-submit-bearer-token",
            "token",
            "--dummy-ol-client",
            "--health-check-host",
            "127.0.0.1",
            "--health-check-port",
            "8081",
            "--db-retry-count",
            "3",
            "--sequencer",
            "--btc-rpc-url",
            "http://localhost:18443",
            "--btc-rpc-user",
            "user",
            "--btc-rpc-password",
            "pass",
            "--l1-reorg-safe-depth",
            "6",
            "--genesis-l1-height",
            "0",
            "--batch-sealing-block-count",
            "100",
            "--chunk-sealing-block-count",
            "50",
            "--chunk-sealing-gas-limit",
            "100000000",
            "--batch-event-channel-capacity",
            "64",
            "--dev-track-latest-epoch",
            "--prover-backend",
            "sp1",
            "--sp1-proof-deadline-secs",
            "60",
            "--prover-program",
            "/tmp/guest-alpen-chunk.elf:/tmp/guest-alpen-acct.elf",
            "--btcio-fee-policy",
            "fixed",
            "--btcio-conf-target",
            "2",
            "--btcio-fee-rate",
            "1.5",
            "--btcio-mempool-base-url",
            "https://mempool.space",
            "--btcio-mempool-tier",
            "fastest",
            "--btcio-retry-count",
            "3",
            "--btcio-retry-interval",
            "500",
            "--beneficiary-address",
            "0x5400000000000000000000000000000000000010",
        ]);

        // Spot-check that renamed fields map back to their pinned flags.
        assert!(config.sequencer.enabled);
        assert_eq!(
            config.sequencer.http_url.as_deref(),
            Some("http://localhost:8545")
        );
        assert_eq!(config.ol.client_url.as_deref(), Some("ws://localhost:8432"));
        assert!(config.ol.dummy_client);
        assert_eq!(config.btcio.fee_policy, BtcioFeePolicyArg::Fixed);
        assert_eq!(config.btcio.fee_rate, Some(1.5));
        assert_eq!(
            config.sequencer.prover.prover_program,
            vec![ProverProgramPaths {
                chunk_path: PathBuf::from("/tmp/guest-alpen-chunk.elf"),
                acct_path: PathBuf::from("/tmp/guest-alpen-acct.elf"),
            }]
        );
    }

    /// `--prover-program` is a plain, unrequired `Vec` field at the clap
    /// level (a fullnode never needs it), so `--prover-backend` defaulting
    /// to `native` with no program parses fine; `ProverArgs::backend()` is
    /// what rejects its absence, once it's known `--sequencer` is actually
    /// in play (see
    /// `prover_backend_tests::missing_prover_program_is_rejected`).
    #[test]
    fn sequencer_parses_without_prover_program() {
        let config = try_parse_additional_config(&[
            "--dummy-ol-client",
            "--sequencer",
            "--btc-rpc-url",
            "http://localhost:18443",
            "--btc-rpc-user",
            "user",
            "--btc-rpc-password",
            "pass",
        ])
        .unwrap();
        assert!(config.sequencer.prover.backend().is_err());
    }

    /// A fullnode (no `--sequencer`) never touches the prover, so none of
    /// the backend-specific flags are required even though
    /// `--prover-backend` defaults to `native`.
    #[test]
    fn fullnode_does_not_require_prover_backend_flags() {
        let config = try_parse_additional_config(&["--dummy-ol-client"]).unwrap();
        assert!(!config.sequencer.enabled);
    }

    /// Passing a `--prover-program` with `--sequencer` (default
    /// `--prover-backend native`) parses; the paths don't need to exist yet
    /// at parse time, only when the backend is actually built (see
    /// `sequencer::prover::backend`).
    #[test]
    fn prover_program_paths_do_not_need_to_exist_at_parse_time() {
        let config = try_parse_additional_config(&[
            "--dummy-ol-client",
            "--sequencer",
            "--btc-rpc-url",
            "http://localhost:18443",
            "--btc-rpc-user",
            "user",
            "--btc-rpc-password",
            "pass",
            "--prover-program",
            "/tmp/native-chunk-signing-key.hex:/tmp/native-acct-signing-key.hex",
        ])
        .unwrap();
        assert!(config.sequencer.prover.backend().is_ok());
    }

    /// `--prover-program` is repeatable: each occurrence adds a candidate,
    /// so an operator can hand the sequencer both the currently-active and
    /// a not-yet-active program across a VK rotation without a restart.
    #[test]
    fn prover_program_flag_is_repeatable() {
        let config = try_parse_additional_config(&[
            "--dummy-ol-client",
            "--sequencer",
            "--btc-rpc-url",
            "http://localhost:18443",
            "--btc-rpc-user",
            "user",
            "--btc-rpc-password",
            "pass",
            "--prover-program",
            "/tmp/chunk-a.hex:/tmp/acct-a.hex",
            "--prover-program",
            "/tmp/chunk-b.hex:/tmp/acct-b.hex",
        ])
        .unwrap();
        assert_eq!(
            config.sequencer.prover.prover_program,
            vec![
                ProverProgramPaths {
                    chunk_path: PathBuf::from("/tmp/chunk-a.hex"),
                    acct_path: PathBuf::from("/tmp/acct-a.hex"),
                },
                ProverProgramPaths {
                    chunk_path: PathBuf::from("/tmp/chunk-b.hex"),
                    acct_path: PathBuf::from("/tmp/acct-b.hex"),
                },
            ]
        );
    }
}

#[cfg(test)]
mod prover_backend_tests {
    use super::*;

    fn program() -> ProverProgramPaths {
        ProverProgramPaths {
            chunk_path: PathBuf::from("/tmp/chunk.elf"),
            acct_path: PathBuf::from("/tmp/acct.elf"),
        }
    }

    fn native_args() -> ProverArgs {
        ProverArgs {
            backend: ProverBackendArg::Native,
            sp1_proof_deadline_secs: None,
            prover_program: vec![program()],
        }
    }

    fn sp1_args() -> ProverArgs {
        ProverArgs {
            backend: ProverBackendArg::Sp1,
            sp1_proof_deadline_secs: None,
            prover_program: vec![program()],
        }
    }

    #[test]
    fn native_backend_resolves_to_native_config() {
        match native_args().backend().unwrap() {
            ProverBackendConfig::Native { programs } => {
                assert_eq!(programs, vec![program()]);
            }
            other => panic!("expected Native, got {other:?}"),
        }
    }

    #[test]
    fn sp1_backend_resolves_to_sp1_config() {
        match sp1_args().backend().unwrap() {
            ProverBackendConfig::Sp1 { programs, .. } => {
                assert_eq!(programs, vec![program()]);
            }
            other => panic!("expected Sp1, got {other:?}"),
        }
    }

    #[test]
    fn missing_prover_program_is_rejected() {
        let mut args = native_args();
        args.prover_program = Vec::new();
        let err = args.backend().unwrap_err();
        assert!(err.to_string().contains("--prover-program"));

        let mut args = sp1_args();
        args.prover_program = Vec::new();
        let err = args.backend().unwrap_err();
        assert!(err.to_string().contains("--prover-program"));
    }

    #[test]
    fn parse_prover_program_paths_splits_on_colon() {
        let p = parse_prover_program_paths("/a/chunk.elf:/a/acct.elf").unwrap();
        assert_eq!(p.chunk_path, PathBuf::from("/a/chunk.elf"));
        assert_eq!(p.acct_path, PathBuf::from("/a/acct.elf"));
    }

    #[test]
    fn parse_prover_program_paths_rejects_missing_separator() {
        assert!(parse_prover_program_paths("/a/chunk.elf").is_err());
    }

    #[test]
    fn parse_prover_program_paths_rejects_empty_chunk_or_acct() {
        assert!(parse_prover_program_paths(":/a/acct.elf").is_err());
        assert!(parse_prover_program_paths("/a/chunk.elf:").is_err());
    }
}

#[cfg(all(test, feature = "sequencer"))]
mod writer_config_tests {
    use bitcoind_async_client::corepc_types::bitcoin::FeeRate;

    use super::*;

    fn args(
        policy: BtcioFeePolicyArg,
        fee_rate: Option<f64>,
        mempool_url: Option<&str>,
    ) -> BtcioArgs {
        BtcioArgs {
            fee_policy: policy,
            conf_target: 1,
            fee_rate,
            mempool_base_url: mempool_url.map(str::to_owned),
            mempool_tier: BtcioMempoolTierArg::Fastest,
            retry_count: DEFAULT_BTCIO_RETRY_COUNT,
            retry_interval: DEFAULT_BTCIO_RETRY_INTERVAL_MS,
        }
    }

    #[test]
    fn fixed_requires_fee_rate() {
        let err = args(BtcioFeePolicyArg::Fixed, None, None)
            .writer_config()
            .unwrap_err();
        assert!(err.to_string().contains("--btcio-fee-rate"));
    }

    #[test]
    fn fixed_one_sat_vb() {
        let cfg = args(BtcioFeePolicyArg::Fixed, Some(1.0), None)
            .writer_config()
            .unwrap();
        assert_eq!(
            cfg.fee_policy(),
            &FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_vb_u32(1)
            }
        );
    }

    #[test]
    fn fixed_half_sat_vb() {
        let cfg = args(BtcioFeePolicyArg::Fixed, Some(0.5), None)
            .writer_config()
            .unwrap();
        assert_eq!(
            cfg.fee_policy(),
            &FeePolicy::Fixed {
                fee_rate: FeeRate::from_sat_per_kwu(125)
            }
        );
    }

    #[test]
    fn mempool_requires_base_url() {
        let err = args(BtcioFeePolicyArg::Mempool, None, None)
            .writer_config()
            .unwrap_err();
        assert!(err.to_string().contains("--btcio-mempool-base-url"));
    }

    #[test]
    fn mempool_rejects_empty_base_url() {
        let err = args(BtcioFeePolicyArg::Mempool, None, Some(""))
            .writer_config()
            .unwrap_err();
        assert!(err.to_string().contains("--btcio-mempool-base-url"));
    }

    #[test]
    fn mempool_with_url_succeeds() {
        let cfg = args(
            BtcioFeePolicyArg::Mempool,
            None,
            Some("https://mempool.space/signet"),
        )
        .writer_config()
        .unwrap();
        match cfg.fee_policy() {
            FeePolicy::MempoolExplorer {
                mempool_base_url, ..
            } => assert_eq!(mempool_base_url, "https://mempool.space/signet"),
            other => panic!("expected MempoolExplorer, got {other:?}"),
        }
    }

    #[test]
    fn bitcoind_uses_conf_target() {
        let mut a = args(BtcioFeePolicyArg::Bitcoind, None, None);
        a.conf_target = 4;
        let cfg = a.writer_config().unwrap();
        assert_eq!(cfg.fee_policy(), &FeePolicy::BitcoinD { conf_target: 4 });
    }
}
