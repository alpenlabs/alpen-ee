//! CLI argument definitions for the alpen-client binary.
//!
//! [`AdditionalConfig`] is the reth CLI extension type plugged into
//! `NodeCommand<AlpenChainSpecParser, AdditionalConfig>`. It is composed of
//! smaller [`clap::Args`] structs flattened into one command, grouped by
//! domain. Flattening does not namespace flags, so every `--long-flag` (and
//! env var) keeps its existing name; the grouping only affects code layout
//! and `--help` section headings.

use std::{fs, path::Path, sync::Arc};

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
    #[arg(long = "ol-client-url")]
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
    #[cfg(feature = "sequencer")]
    #[arg(long, required = false)]
    pub batch_event_channel_capacity: Option<usize>,

    /// Use the zkaleido `NativeHost` for the EE chunk + acct provers
    /// instead of the SP1 remote host.
    ///
    /// Dev/test only: skips real Groth16 proving and the compiled guest
    /// ELFs. Functional tests enable this so the sequencer can start
    /// without the SP1 prover ELFs present on disk.
    #[arg(long, default_value_t = false)]
    pub dev_native_prover: bool,

    /// End-to-end deadline (seconds) passed to the SP1 prover network on
    /// every chunk/acct proof request. Only used with the remote SP1
    /// backend. When unset, a built-in default is applied (see
    /// `DEFAULT_SP1_DEADLINE_SECS`).
    #[arg(long, required = false)]
    pub sp1_proof_deadline_secs: Option<u64>,

    #[cfg(feature = "sequencer")]
    #[arg(long, default_value_t = DEFAULT_BENEFICIARY_ADDRESS)]
    pub beneficiary_address: Address,
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
    #[arg(long = "btcio-fee-rate")]
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

    fn parse_additional_config(args: &[&str]) -> AdditionalConfig {
        let params_fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/alpen-params.json");
        let mut argv = vec![
            "alpen-client",
            "--alpen-params",
            params_fixture,
            "--sequencer-pubkey",
            SEQUENCER_PUBKEY,
        ];
        argv.extend_from_slice(args);
        <AdditionalConfig as clap::Parser>::parse_from(argv)
    }

    /// The artifact loads at CLI parse time and the genesis facts are
    /// derived from its embedded EVM spec.
    #[test]
    fn alpen_params_flag_loads_the_artifact() {
        let config = parse_additional_config(&[]);

        assert_eq!(config.chain.alpen_params.genesis_block_info().blocknum(), 0);
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
            "--dev-native-prover",
            "--dev-track-latest-epoch",
            "--sp1-proof-deadline-secs",
            "60",
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
