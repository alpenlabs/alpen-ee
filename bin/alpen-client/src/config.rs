//! Typed configuration loaded from `--alpen-config <PATH>`.
//!
//! Two types with unambiguous responsibilities: [`AlpenClientConfigFile`] is
//! the public TOML file format (the only type that round-trips through
//! `toml`), [`AlpenClientConfig`] is the validated runtime representation the
//! rest of the binary uses. [`AlpenClientConfig::from_toml_str`] is the sole
//! entry point between the two; [`AlpenClientConfigFile`] never leaves this
//! module.

use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
};

use alloy_primitives::{address, Address};
use alpen_ee_ol_tracker::EpochTrackingMode;
#[cfg(feature = "sequencer")]
use alpen_ee_params::AlpenParams;
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};
use strata_config::{btcio::L1FeePolicyConfig, BitcoindConfig};
use strata_primitives::{buf::Buf32, L1Height};

#[cfg(feature = "sequencer")]
use crate::sequencer::da_fee_rate::validate_config;

// Applied when the matching TOML field is omitted.
const DEFAULT_HEALTH_CHECK_HOST: &str = "0.0.0.0";
const DEFAULT_HEALTH_CHECK_PORT: u16 = 8080;
const DEFAULT_BENEFICIARY_ADDRESS: Address = address!("5400000000000000000000000000000000000010");
const DEFAULT_L1_REORG_SAFE_DEPTH: u32 = 6;
const DEFAULT_BATCH_SEALING_BLOCK_COUNT: u64 = 100;
const DEFAULT_DB_RETRY_COUNT: u16 = 5;
const DEFAULT_BLOCKTIME_MS: NonZeroU64 = NonZeroU64::new(5_000).expect("5000 is always NonZero");
const DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(64).expect("64 is always NonZero");

fn default_health_check_host() -> String {
    DEFAULT_HEALTH_CHECK_HOST.to_owned()
}

fn default_health_check_port() -> u16 {
    DEFAULT_HEALTH_CHECK_PORT
}

fn default_db_retry_count() -> u16 {
    DEFAULT_DB_RETRY_COUNT
}

fn default_l1_reorg_safe_depth() -> u32 {
    DEFAULT_L1_REORG_SAFE_DEPTH
}

fn default_beneficiary_address() -> Address {
    DEFAULT_BENEFICIARY_ADDRESS
}

fn default_blocktime_ms() -> NonZeroU64 {
    DEFAULT_BLOCKTIME_MS
}

fn default_batch_sealing_block_count() -> u64 {
    DEFAULT_BATCH_SEALING_BLOCK_COUNT
}

fn default_batch_event_channel_capacity() -> NonZeroUsize {
    DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY
}

/// Deserializes a [`Buf32`] from hex, with or without an `0x` prefix.
///
/// [`Buf32`]'s derived [`Deserialize`] goes through the `hex` crate's serde
/// helper, which rejects the prefix. Its [`FromStr`] accepts it, and that is
/// what reads `SEQUENCER_PRIVATE_KEY` from the environment, so parsing
/// through [`FromStr`] keeps every hand-typed key the same shape.
///
/// [`FromStr`]: std::str::FromStr
fn buf32_from_hex<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Buf32, D::Error> {
    let s = String::deserialize(deserializer)?;
    s.parse::<Buf32>().map_err(DeError::custom)
}

/// Returns every input key that the typed config did not consume.
///
/// Comparing the parsed input with the typed config's serialized form keeps
/// unknown-field detection effective across [`OlConfig`]'s flattened enum and
/// the config types reused from `strata_config`, where
/// `#[serde(deny_unknown_fields)]` cannot be added locally.
fn unknown_field_paths(input: &toml::Value, typed: &toml::Value) -> Vec<String> {
    fn collect(
        input: &toml::Value,
        typed: Option<&toml::Value>,
        path: &str,
        out: &mut Vec<String>,
    ) {
        match input {
            toml::Value::Table(input_table) => {
                let typed_table = typed.and_then(toml::Value::as_table);
                for (key, input_value) in input_table {
                    let field_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    let Some(typed_value) = typed_table.and_then(|table| table.get(key)) else {
                        out.push(field_path);
                        continue;
                    };
                    collect(input_value, Some(typed_value), &field_path, out);
                }
            }
            toml::Value::Array(input_values) => {
                let typed_values = typed.and_then(toml::Value::as_array);
                for (index, input_value) in input_values.iter().enumerate() {
                    let item_path = format!("{path}[{index}]");
                    let Some(typed_value) = typed_values.and_then(|values| values.get(index))
                    else {
                        out.push(item_path);
                        continue;
                    };
                    collect(input_value, Some(typed_value), &item_path, out);
                }
            }
            _ => {}
        }
    }

    let mut unknown_fields = Vec::new();
    collect(input, Some(typed), "", &mut unknown_fields);
    unknown_fields
}

/// The TOML file format. Private: [`AlpenClientConfig::from_toml_str`] is the
/// only place this type is constructed or consumed.
// Field order matters here, not just for readability: TOML requires every
// plain `key = value` pair to precede any `[table]` section, and serde's
// toml serializer emits fields in declaration order, so all scalar fields
// must come before `ol`/`full_node`/`sequencer` (each of which serializes as
// a table) — reordering them any other way fails to serialize at all
// (`ValueAfterTable`), which the round-trip tests in this module catch.
#[derive(Debug, Serialize, Deserialize)]
struct AlpenClientConfigFile {
    #[serde(default = "default_health_check_host")]
    health_check_host: String,
    #[serde(default = "default_health_check_port")]
    health_check_port: u16,
    #[serde(default = "default_db_retry_count")]
    db_retry_count: u16,

    // Rollup-to-L1 facts, not sequencer *preferences*: every honest participant needs the
    // same values — unlike batch_sealing_block_count etc., not a sequencer-operator tuning
    // knob. Longer-term these arguably belong in `AlpenParams` itself (chain-wide, never
    // operator-configurable) rather than per-node config; out of scope here since that
    // artifact is consumed well beyond alpen-client (ASM/OL, ZK guests).
    #[serde(default = "default_l1_reorg_safe_depth")]
    l1_reorg_safe_depth: u32,
    #[serde(default)]
    genesis_l1_height: L1Height,

    mode: NodeModeTag,

    ol: OlConfig,
    full_node: Option<FullNodeConfig>,
    #[serde(default)]
    sequencer: Option<SequencerConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NodeModeTag {
    FullNode,
    Sequencer,
}

/// OL connection config: which client to use, plus tracker behavior that
/// belongs with it rather than floating as an unrelated top-level field.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OlConfig {
    #[serde(flatten)]
    pub(crate) source: OlSource,
    /// Which OL epoch the chain tracker advances against. Defaults to the
    /// canonical `confirmed` epoch (CSM-based); `latest` (the newest epoch
    /// completed by the connected Strata node, not yet checkpointed on L1)
    /// is dev/test only.
    #[serde(default)]
    pub(crate) epoch_tracking_mode: EpochTrackingMode,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub(crate) enum OlSource {
    /// Use a dummy OL client instead of connecting to a real OL node. For
    /// testing EE functionality in isolation.
    Dummy,
    Rpc {
        /// URL of the OL node RPC (`http[s]://` or `ws[s]://`).
        client_url: String,
    },
}

/// The resolved runtime representation the rest of the binary uses (params
/// comes from the separate `--alpen-params` flag). Plain struct, no Serde
/// derive: [`AlpenClientConfigFile`] is the only type that round-trips
/// through TOML.
#[derive(Debug)]
pub(crate) struct AlpenClientConfig {
    pub(crate) health_check_host: String,
    pub(crate) health_check_port: u16,
    pub(crate) db_retry_count: u16,
    pub(crate) ol: OlConfig,
    pub(crate) mode: NodeMode,
}

#[cfg_attr(
    feature = "sequencer",
    expect(
        clippy::large_enum_variant,
        reason = "one long-lived config value; size difference does not matter"
    )
)]
#[derive(Debug)]
pub(crate) enum NodeMode {
    FullNode(FullNodeConfig),
    #[cfg(feature = "sequencer")]
    Sequencer(SequencerMode),
}

/// Everything the sequencer path reads out of config.
///
/// The `[sequencer]` table plus the two top-level L1 keys that only the
/// sequencer's DA pipeline consumes. They stay top-level in the TOML file
/// (see [`AlpenClientConfigFile`]) because they describe the rollup's
/// relationship to L1 rather than an operator preference, but nothing on the
/// full-node path reads them, so the runtime config keeps them here instead
/// of on [`AlpenClientConfig`].
#[cfg(feature = "sequencer")]
#[derive(Debug)]
pub(crate) struct SequencerMode {
    pub(crate) config: SequencerConfig,
    pub(crate) l1_reorg_safe_depth: u32,
    pub(crate) genesis_l1_height: L1Height,
}

/// `[sequencer.prover]` — which EE chunk/acct prover backend to run.
///
/// Tagged on `backend`, so each backend names only the fields it needs and
/// serde rejects a config that omits one. That replaces the old bool plus
/// separately-optional paths, where "these paths are required unless
/// native" was a rule every layer had to re-check.
///
/// Holds paths rather than file contents: reading is left to whoever
/// actually builds the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub(crate) enum ProverBackendConfig {
    /// zkaleido `NativeHost`, signing chunk/acct proofs with the keys read
    /// from the given files instead of doing real ZK proving.
    ///
    /// The account key has to match whatever the OL genesis `update_vk`
    /// expects, or the predicate-key check at startup fails.
    Native {
        chunk_signing_key_path: PathBuf,
        acct_signing_key_path: PathBuf,
    },
    /// SP1 remote host. Needs the `sp1` feature compiled in.
    Sp1 {
        /// Falls back to `DEFAULT_SP1_DEADLINE_SECS` when unset, so nothing
        /// is resolved here.
        deadline_secs: Option<u64>,
        /// Paths to the compiled SP1 guest ELFs. Explicit so one
        /// `alpen-client` build can run against different guest ELFs
        /// without a rebuild.
        chunk_elf_path: PathBuf,
        acct_elf_path: PathBuf,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FullNodeConfig {
    /// The key every gossiped block must be signed with.
    #[serde(deserialize_with = "buf32_from_hex")]
    pub(crate) sequencer_pubkey: Buf32,
    /// The sequencer's HTTP RPC endpoint, used to forward transactions
    /// submitted to this node.
    ///
    /// When set, `eth_sendRawTransaction` posts the raw transaction to that
    /// endpoint and adds it to the local pool. The bytes go out before this
    /// node checks anything past the encoding and the signature. The sequencer
    /// applies the nonce, balance and fee checks itself, exactly as it would
    /// for a direct submission, so it and the local pool can disagree on
    /// whether to accept a transaction.
    ///
    /// When unset, the transaction only enters the local pool and reaches the
    /// sequencer over reth's P2P transaction gossip. That handles the common
    /// case, but two cases slip through it. Only executable transactions are
    /// gossiped, so one with a future nonce waits in the local queue until the
    /// gap fills. Gossip also needs a live peer path to the sequencer, which a
    /// sparsely peered node may not have. Forwarding covers both.
    pub(crate) sequencer_http_url: Option<String>,
}

/// Selects the source that recommends the sequencer's DA fee rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaFeeRatePolicyConfig {
    /// Reuses the Bitcoin writer's configured L1 fee policy.
    WriterBacked,
    /// Returns one operator-configured rate without consulting Bitcoin.
    Fixed { rate_wei_per_byte: u64 },
}

/// Validated contents of `[sequencer.da_fee_rate]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DaFeeRateConfig {
    /// Chooses the policy that supplies unadjusted rate recommendations.
    pub(crate) policy: DaFeeRatePolicyConfig,
    /// Seeds the controller until its first successful policy fetch.
    pub(crate) fallback_policy_rate_wei_per_byte: u64,
    /// Controls how often the selected policy is queried.
    pub(crate) refresh_interval_seconds: NonZeroU64,
    /// Marks a dynamic rate stale after this long without a successful fetch.
    pub(crate) stale_after_seconds: NonZeroU64,
    /// Scales policy rates in basis points before applying the offset.
    pub(crate) multiplier_bps: u64,
    /// Adds a fixed wei-per-byte amount after scaling.
    pub(crate) offset_wei_per_byte: u64,
}

/// Serde representation of [`DaFeeRateConfig`].
///
/// The checked runtime type uses an enum so `fixed_rate_wei_per_byte` cannot
/// exist for a writer-backed policy and cannot be absent for a fixed policy.
#[derive(Serialize, Deserialize)]
struct DaFeeRateConfigFile {
    #[serde(default)]
    policy: DaFeeRatePolicyTag,
    #[serde(skip_serializing_if = "Option::is_none")]
    fixed_rate_wei_per_byte: Option<u64>,
    fallback_policy_rate_wei_per_byte: u64,
    refresh_interval_seconds: NonZeroU64,
    stale_after_seconds: NonZeroU64,
    #[serde(default = "default_da_fee_rate_multiplier_bps")]
    multiplier_bps: u64,
    #[serde(default)]
    offset_wei_per_byte: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DaFeeRatePolicyTag {
    #[default]
    WriterBacked,
    Fixed,
}

const fn default_da_fee_rate_multiplier_bps() -> u64 {
    10_000
}

impl TryFrom<DaFeeRateConfigFile> for DaFeeRateConfig {
    type Error = String;

    fn try_from(raw: DaFeeRateConfigFile) -> Result<Self, Self::Error> {
        let policy = match (raw.policy, raw.fixed_rate_wei_per_byte) {
            (DaFeeRatePolicyTag::WriterBacked, None) => DaFeeRatePolicyConfig::WriterBacked,
            (DaFeeRatePolicyTag::WriterBacked, Some(_)) => {
                return Err(
                    "fixed_rate_wei_per_byte is only valid when policy = \"fixed\"".to_owned(),
                );
            }
            (DaFeeRatePolicyTag::Fixed, Some(rate_wei_per_byte)) => {
                DaFeeRatePolicyConfig::Fixed { rate_wei_per_byte }
            }
            (DaFeeRatePolicyTag::Fixed, None) => {
                return Err(
                    "fixed_rate_wei_per_byte is required when policy = \"fixed\"".to_owned(),
                );
            }
        };
        if raw.stale_after_seconds < raw.refresh_interval_seconds {
            return Err("stale_after_seconds must be at least refresh_interval_seconds".to_owned());
        }

        Ok(Self {
            policy,
            fallback_policy_rate_wei_per_byte: raw.fallback_policy_rate_wei_per_byte,
            refresh_interval_seconds: raw.refresh_interval_seconds,
            stale_after_seconds: raw.stale_after_seconds,
            multiplier_bps: raw.multiplier_bps,
            offset_wei_per_byte: raw.offset_wei_per_byte,
        })
    }
}

impl From<&DaFeeRateConfig> for DaFeeRateConfigFile {
    fn from(config: &DaFeeRateConfig) -> Self {
        let (policy, fixed_rate_wei_per_byte) = match config.policy {
            DaFeeRatePolicyConfig::WriterBacked => (DaFeeRatePolicyTag::WriterBacked, None),
            DaFeeRatePolicyConfig::Fixed { rate_wei_per_byte } => {
                (DaFeeRatePolicyTag::Fixed, Some(rate_wei_per_byte))
            }
        };
        Self {
            policy,
            fixed_rate_wei_per_byte,
            fallback_policy_rate_wei_per_byte: config.fallback_policy_rate_wei_per_byte,
            refresh_interval_seconds: config.refresh_interval_seconds,
            stale_after_seconds: config.stale_after_seconds,
            multiplier_bps: config.multiplier_bps,
            offset_wei_per_byte: config.offset_wei_per_byte,
        }
    }
}

impl Serialize for DaFeeRateConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DaFeeRateConfigFile::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DaFeeRateConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        DaFeeRateConfigFile::deserialize(deserializer)?
            .try_into()
            .map_err(DeError::custom)
    }
}

// The `sequencer` feature gate sits on `SequencerMode`, not on this struct or its fields.
// The definition stays unconditional because `AlpenClientConfigFile` names it in every build,
// the same way it names `BitcoindConfig` and `L1FeePolicyConfig`. A slim build can therefore
// parse a `[sequencer]` table and reject it, while never constructing a live sequencer mode.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SequencerConfig {
    #[serde(default = "default_beneficiary_address")]
    pub(crate) beneficiary_address: Address,
    /// How long the sequencer waits between blocks.
    #[serde(default = "default_blocktime_ms")]
    pub(crate) blocktime_ms: NonZeroU64,
    #[serde(default = "default_batch_sealing_block_count")]
    pub(crate) batch_sealing_block_count: u64,
    /// Omitting this falls back to `batch_sealing_block_count`, a sibling
    /// field rather than a constant, which no serde default can express.
    /// [`SequencerConfig::chunk_sealing_block_count`] applies the fallback.
    pub(crate) chunk_sealing_block_count: Option<u64>,
    /// `None` turns the gas-limit sealing policy off, leaving chunks to seal
    /// on block count alone. No number expresses that, which is why this
    /// takes no default.
    pub(crate) chunk_sealing_gas_limit: Option<u64>,
    /// Non-zero because `mpsc::channel` panics on a zero-capacity buffer.
    #[serde(default = "default_batch_event_channel_capacity")]
    pub(crate) batch_event_channel_capacity: NonZeroUsize,
    /// URL of the authenticated OL transaction submission RPC.
    ///
    /// Required, and checked by [`AlpenClientConfig`]. The [`Option`] covers
    /// exactly one case: [`OlSource::Dummy`], where there is no OL node to
    /// submit to.
    ///
    /// Sits here rather than beside `client_url` in [`OlConfig`] because only
    /// a sequencer submits to OL. A full node has no `[sequencer]` table, so
    /// it cannot express this at all.
    ///
    /// The bearer token that authenticates submission is a secret, so it is
    /// read from `STRATA_SUBMIT_RPC_TOKEN` rather than being a field here.
    pub(crate) ol_submit_url: Option<String>,
    /// `[sequencer.prover]` — the EE chunk/acct prover backend.
    pub(crate) prover: ProverBackendConfig,
    /// `[sequencer.bitcoind]` — reused verbatim from `strata_config`.
    // TODO(STR-4177): stop configuring `rpc_user`/`rpc_password` as plaintext TOML fields.
    pub(crate) bitcoind: BitcoindConfig,
    /// `[sequencer.l1_fee_policy]` — reused verbatim from `strata_config::btcio`.
    pub(crate) l1_fee_policy: L1FeePolicyConfig,
    /// `[sequencer.da_fee_rate]` — policy selection and controller timing.
    pub(crate) da_fee_rate: DaFeeRateConfig,
}

// Only the sequencer path calls these, so they can be gated even though the
// struct itself cannot be.
#[cfg(feature = "sequencer")]
impl SequencerConfig {
    pub(crate) fn chunk_sealing_block_count(&self) -> u64 {
        self.chunk_sealing_block_count
            .unwrap_or(self.batch_sealing_block_count)
    }

    /// Checks `chunk_sealing_gas_limit` against the genesis block gas limit.
    ///
    /// A chunk has to fit at least one block, so the configured budget must
    /// clear the per-block limit. EIP-1559 lets that limit drift from genesis
    /// by ±1/1024 per block, so the floor is 2× genesis: loose enough to
    /// absorb the drift, tight enough to catch an obviously wrong value.
    ///
    /// This needs [`AlpenParams`], which arrives from a separate file, so it
    /// runs at startup rather than during deserialization.
    pub(crate) fn validate_chunk_sealing_gas_limit(
        &self,
        params: &AlpenParams,
    ) -> eyre::Result<()> {
        let Some(configured) = self.chunk_sealing_gas_limit else {
            return Ok(());
        };

        let genesis_gas_limit = params.evm_spec().genesis().gas_limit;
        let min_chunk_gas = genesis_gas_limit.saturating_mul(2);
        eyre::ensure!(
            configured >= min_chunk_gas,
            "sequencer.chunk_sealing_gas_limit ({configured}) is below the minimum \
             ({min_chunk_gas}, 2× genesis block gas limit {genesis_gas_limit}). \
             A single block can use up to the per-block gas limit, so the chunk \
             budget must be large enough to always fit at least one block.",
        );
        Ok(())
    }
}

impl TryFrom<AlpenClientConfigFile> for AlpenClientConfig {
    type Error = eyre::Report;

    fn try_from(raw: AlpenClientConfigFile) -> eyre::Result<Self> {
        // Only the table matching `mode` may be present. Accepting the other one and
        // dropping it would let a stale or copy-pasted table read as live: an operator
        // could see proving and OL submission configured under `[sequencer]` while the
        // process runs as a full node that never looks at either.
        let mode = match raw.mode {
            NodeModeTag::FullNode => {
                eyre::ensure!(
                    raw.sequencer.is_none(),
                    "[sequencer] table is not allowed when mode = \"full_node\""
                );
                let fc = raw.full_node.ok_or_else(|| {
                    eyre::eyre!("[full_node] table required when mode = \"full_node\"")
                })?;
                NodeMode::FullNode(fc)
            }
            NodeModeTag::Sequencer => {
                eyre::ensure!(
                    raw.full_node.is_none(),
                    "[full_node] table is not allowed when mode = \"sequencer\""
                );
                #[cfg(not(feature = "sequencer"))]
                {
                    eyre::bail!(
                        "mode = \"sequencer\" requires the `sequencer` feature; \
                         rebuild with default features"
                    );
                }
                #[cfg(feature = "sequencer")]
                {
                    let seq = raw.sequencer.ok_or_else(|| {
                        eyre::eyre!("[sequencer] table required when mode = \"sequencer\"")
                    })?;
                    validate_config(&seq.da_fee_rate)?;
                    NodeMode::Sequencer(SequencerMode {
                        config: seq,
                        l1_reorg_safe_depth: raw.l1_reorg_safe_depth,
                        genesis_l1_height: raw.genesis_l1_height,
                    })
                }
            }
        };

        // The one rule left that no field attribute can express, because it spans the
        // `[sequencer]` table and the `[ol]` one: a sequencer pointed at a real OL node has
        // to say where it submits. Against `OlSource::Dummy` there is nothing to submit to.
        // A slim (non-`sequencer`) build can only ever be a full node, which has no
        // `[sequencer]` table and so cannot reach this at all.
        #[cfg(feature = "sequencer")]
        if let (NodeMode::Sequencer(seq), OlSource::Rpc { .. }) = (&mode, &raw.ol.source) {
            eyre::ensure!(
                seq.config.ol_submit_url.is_some(),
                "sequencer.ol_submit_url is required unless ol.source = \"dummy\""
            );
        }

        Ok(Self {
            health_check_host: raw.health_check_host,
            health_check_port: raw.health_check_port,
            db_retry_count: raw.db_retry_count,
            ol: raw.ol,
            mode,
        })
    }
}

impl AlpenClientConfig {
    /// The sole entry point from a TOML file to a validated config —
    /// [`AlpenClientConfigFile`] is a private implementation detail of this
    /// function, never returned or accepted anywhere else, so nothing
    /// outside this function can hold a config whose `mode`/`ol`
    /// combination hasn't already been checked.
    pub(crate) fn from_toml_str(contents: &str) -> eyre::Result<Self> {
        let input: toml::Value = toml::from_str(contents)?;
        let file: AlpenClientConfigFile = input.clone().try_into()?;
        let typed = toml::Value::try_from(&file)?;
        let unknown_fields = unknown_field_paths(&input, &typed);
        eyre::ensure!(
            unknown_fields.is_empty(),
            "unknown field(s): {}",
            unknown_fields.join(", ")
        );
        file.try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_NODE_TOML: &str = include_str!("../testdata/config.full_node.toml");
    const SEQUENCER_TOML: &str = include_str!("../testdata/config.sequencer.toml");

    // The configs the docker composes mount as `--alpen-config`. Checked here
    // so they can't drift from the schema unnoticed -- nothing else in CI
    // starts those containers.
    const DOCKER_TOMLS: &[(&str, &str)] = &[
        (
            "docker/configs/p2p-test-node-a.toml",
            include_str!("../../../docker/configs/p2p-test-node-a.toml"),
        ),
        (
            "docker/configs/p2p-test-node-b.toml",
            include_str!("../../../docker/configs/p2p-test-node-b.toml"),
        ),
        #[cfg(feature = "sequencer")]
        (
            "docker/configs/eest-sequencer.toml",
            include_str!("../../../docker/configs/eest-sequencer.toml"),
        ),
    ];

    #[test]
    fn docker_configs_resolve() {
        for (path, contents) in DOCKER_TOMLS {
            AlpenClientConfig::from_toml_str(contents)
                .unwrap_or_else(|e| panic!("{path} failed to load: {e}"));
        }
    }

    #[test]
    fn full_node_toml_round_trips() {
        let file: AlpenClientConfigFile = toml::from_str(FULL_NODE_TOML).unwrap();
        let serialized = toml::to_string(&file).unwrap();
        let reparsed: AlpenClientConfigFile = toml::from_str(&serialized).unwrap();
        assert_eq!(
            toml::to_string(&reparsed).unwrap(),
            serialized,
            "full_node config should round-trip byte-for-byte after one hop"
        );
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn sequencer_toml_round_trips() {
        let file: AlpenClientConfigFile = toml::from_str(SEQUENCER_TOML).unwrap();
        let serialized = toml::to_string(&file).unwrap();
        let reparsed: AlpenClientConfigFile = toml::from_str(&serialized).unwrap();
        assert_eq!(
            toml::to_string(&reparsed).unwrap(),
            serialized,
            "sequencer config should round-trip byte-for-byte after one hop"
        );
    }

    #[test]
    fn full_node_toml_resolves() {
        let config = AlpenClientConfig::from_toml_str(FULL_NODE_TOML).unwrap();
        assert!(matches!(config.mode, NodeMode::FullNode(_)));
        let NodeMode::FullNode(fc) = &config.mode else {
            unreachable!()
        };
        assert!(fc.sequencer_http_url.is_none());
        assert!(matches!(config.ol.source, OlSource::Rpc { .. }));
    }

    #[test]
    fn sequencer_pubkey_accepts_0x_prefix() {
        let toml = r#"
            mode = "full_node"
            [ol]
            source = "dummy"
            [full_node]
            sequencer_pubkey = "0x1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        "#;
        let config = AlpenClientConfig::from_toml_str(toml).unwrap();
        let NodeMode::FullNode(fc) = &config.mode else {
            unreachable!()
        };
        assert_eq!(
            fc.sequencer_pubkey,
            "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
                .parse()
                .unwrap()
        );
    }

    /// A leftover table for the mode the node is not running in must not parse
    /// quietly, or its settings look active when nothing reads them.
    #[test]
    fn inactive_mode_table_is_rejected() {
        let full_node_with_sequencer = r#"
            mode = "full_node"
            [ol]
            source = "dummy"
            [full_node]
            sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
            [sequencer]
            [sequencer.prover]
            backend = "native"
            chunk_signing_key_path = "/tmp/chunk.key"
            acct_signing_key_path = "/tmp/acct.key"
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "bitcoind"
            [sequencer.da_fee_rate]
            fallback_policy_rate_wei_per_byte = 0
            refresh_interval_seconds = 60
            stale_after_seconds = 300
        "#;
        let err = AlpenClientConfig::from_toml_str(full_node_with_sequencer).unwrap_err();
        assert!(err.to_string().contains("[sequencer] table"), "{err}");

        let sequencer_with_full_node = r#"
            mode = "sequencer"
            [ol]
            source = "dummy"
            [full_node]
            sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
            [sequencer]
            [sequencer.prover]
            backend = "native"
            chunk_signing_key_path = "/tmp/chunk.key"
            acct_signing_key_path = "/tmp/acct.key"
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "bitcoind"
            [sequencer.da_fee_rate]
            fallback_policy_rate_wei_per_byte = 0
            refresh_interval_seconds = 60
            stale_after_seconds = 300
        "#;
        let err = AlpenClientConfig::from_toml_str(sequencer_with_full_node).unwrap_err();
        assert!(err.to_string().contains("[full_node] table"), "{err}");
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let toml = FULL_NODE_TOML.replace("db_retry_count", "db_retry_counts");
        let err = AlpenClientConfig::from_toml_str(&toml).unwrap_err();
        assert!(err.to_string().contains("db_retry_counts"));
    }

    #[test]
    fn unknown_flattened_ol_field_is_rejected() {
        let toml = FULL_NODE_TOML.replace("epoch_tracking_mode", "epoch_tracking_mod");
        let err = AlpenClientConfig::from_toml_str(&toml).unwrap_err();
        assert!(err.to_string().contains("ol.epoch_tracking_mod"));
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn unknown_nested_upstream_config_field_is_rejected() {
        let toml = SEQUENCER_TOML.replace(
            "network = \"regtest\"",
            "network = \"regtest\"\nretry_counts = 3",
        );
        let err = AlpenClientConfig::from_toml_str(&toml).unwrap_err();
        assert!(err.to_string().contains("sequencer.bitcoind.retry_counts"));
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn sequencer_toml_resolves() {
        let config = AlpenClientConfig::from_toml_str(SEQUENCER_TOML).unwrap();
        let NodeMode::Sequencer(seq) = &config.mode else {
            panic!("expected sequencer mode");
        };
        assert_eq!(seq.config.blocktime_ms.get(), 5_000);
        assert_eq!(seq.config.batch_sealing_block_count, 100);
        assert_eq!(seq.config.chunk_sealing_block_count(), 100);
        assert_eq!(
            seq.config.da_fee_rate.policy,
            DaFeeRatePolicyConfig::WriterBacked
        );
        assert_eq!(seq.config.da_fee_rate.multiplier_bps, 10_000);
        assert_eq!(seq.config.da_fee_rate.offset_wei_per_byte, 0);
    }

    #[test]
    fn da_fee_rate_policy_shape_is_checked_during_deserialization() {
        let fixed: DaFeeRateConfig = toml::from_str(
            r#"
            policy = "fixed"
            fixed_rate_wei_per_byte = 17
            fallback_policy_rate_wei_per_byte = 11
            refresh_interval_seconds = 5
            stale_after_seconds = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            fixed.policy,
            DaFeeRatePolicyConfig::Fixed {
                rate_wei_per_byte: 17
            }
        );

        for (config, expected) in [
            (
                r#"
                policy = "fixed"
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 5
                stale_after_seconds = 10
                "#,
                "fixed_rate_wei_per_byte is required",
            ),
            (
                r#"
                policy = "writer_backed"
                fixed_rate_wei_per_byte = 17
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 5
                stale_after_seconds = 10
                "#,
                "fixed_rate_wei_per_byte is only valid",
            ),
            (
                r#"
                policy = "writer_backed"
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 10
                stale_after_seconds = 5
                "#,
                "stale_after_seconds must be at least",
            ),
        ] {
            let error = toml::from_str::<DaFeeRateConfig>(config).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn da_fee_rate_nonzero_and_policy_values_are_checked_by_serde() {
        for (name, config) in [
            (
                "refresh_interval_seconds",
                r#"
                policy = "writer_backed"
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 0
                stale_after_seconds = 10
                "#,
            ),
            (
                "stale_after_seconds",
                r#"
                policy = "writer_backed"
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 5
                stale_after_seconds = 0
                "#,
            ),
            (
                "policy",
                r#"
                policy = "unsupported"
                fallback_policy_rate_wei_per_byte = 11
                refresh_interval_seconds = 5
                stale_after_seconds = 10
                "#,
            ),
        ] {
            assert!(toml::from_str::<DaFeeRateConfig>(config).is_err(), "{name}");
        }
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn configured_startup_rates_must_fit_after_adjustment() {
        fn with_da_fee_rate(table: &str) -> String {
            let prefix = SEQUENCER_TOML
                .split_once("[sequencer.da_fee_rate]")
                .expect("fixture contains DA fee-rate config")
                .0;
            format!("{prefix}[sequencer.da_fee_rate]\n{table}")
        }

        let fallback_overflow = with_da_fee_rate(
            r#"
            policy = "writer_backed"
            fallback_policy_rate_wei_per_byte = 9223372036854775807
            refresh_interval_seconds = 5
            stale_after_seconds = 10
            multiplier_bps = 20001
            "#,
        );
        let error = AlpenClientConfig::from_toml_str(&fallback_overflow).unwrap_err();
        assert!(error.to_string().contains("fallback"), "{error}");

        let fixed_overflow = with_da_fee_rate(
            r#"
            policy = "fixed"
            fixed_rate_wei_per_byte = 9223372036854775807
            fallback_policy_rate_wei_per_byte = 0
            refresh_interval_seconds = 5
            stale_after_seconds = 10
            multiplier_bps = 20001
            "#,
        );
        let error = AlpenClientConfig::from_toml_str(&fixed_overflow).unwrap_err();
        assert!(error.to_string().contains("fixed"), "{error}");
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn da_fee_rate_table_is_required_and_rejects_unknown_fields() {
        let without_table = SEQUENCER_TOML
            .split_once("[sequencer.da_fee_rate]")
            .expect("fixture contains DA fee-rate config")
            .0;
        let error = AlpenClientConfig::from_toml_str(without_table).unwrap_err();
        assert!(error.to_string().contains("da_fee_rate"), "{error}");

        let with_unknown = SEQUENCER_TOML.replace(
            "stale_after_seconds = 300",
            "stale_after_seconds = 300\nrefresh_intervals_seconds = 5",
        );
        let error = AlpenClientConfig::from_toml_str(&with_unknown).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sequencer.da_fee_rate.refresh_intervals_seconds"),
            "{error}"
        );
    }

    /// Each backend names only its own fields, so serde rejects a config
    /// that omits one rather than the failure surfacing at prover startup.
    #[cfg(feature = "sequencer")]
    #[test]
    fn prover_backend_table_is_tagged_on_the_backend() {
        fn sequencer_toml(prover: &str) -> String {
            format!(
                r#"
                mode = "sequencer"
                [ol]
                source = "dummy"
                [sequencer]
                [sequencer.prover]
                {prover}
                [sequencer.bitcoind]
                rpc_url = "http://bitcoind:18443"
                rpc_user = "user"
                rpc_password = "pass"
                network = "regtest"
                [sequencer.l1_fee_policy]
                fee_policy = "bitcoind"
                [sequencer.da_fee_rate]
                fallback_policy_rate_wei_per_byte = 0
                refresh_interval_seconds = 60
                stale_after_seconds = 300
            "#
            )
        }

        fn prover_backend(prover: &str) -> eyre::Result<ProverBackendConfig> {
            let config = AlpenClientConfig::from_toml_str(&sequencer_toml(prover))?;
            let NodeMode::Sequencer(seq) = config.mode else {
                panic!("expected sequencer mode");
            };
            Ok(seq.config.prover)
        }

        let native = prover_backend(
            r#"
            backend = "native"
            chunk_signing_key_path = "/tmp/chunk.key"
            acct_signing_key_path = "/tmp/acct.key"
            "#,
        )
        .unwrap();
        let ProverBackendConfig::Native {
            chunk_signing_key_path,
            acct_signing_key_path,
        } = native
        else {
            panic!("expected the native backend");
        };
        assert_eq!(chunk_signing_key_path, PathBuf::from("/tmp/chunk.key"));
        assert_eq!(acct_signing_key_path, PathBuf::from("/tmp/acct.key"));

        let err = prover_backend(
            r#"
            backend = "sp1"
            acct_elf_path = "/tmp/acct.elf"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("chunk_elf_path"), "{err}");

        // A native key path means nothing to the sp1 backend, so it reads as
        // the unknown field it is rather than being dropped.
        let err = prover_backend(
            r#"
            backend = "sp1"
            chunk_elf_path = "/tmp/chunk.elf"
            acct_elf_path = "/tmp/acct.elf"
            acct_signing_key_path = "/tmp/acct.key"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("sequencer.prover.acct_signing_key_path"),
            "{err}"
        );
    }

    #[test]
    fn ol_client_url_required_unless_dummy() {
        let toml = r#"
            mode = "full_node"
            [ol]
            source = "rpc"
            [full_node]
            sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        "#;
        let err = AlpenClientConfig::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("client_url") || err.to_string().contains("missing"));
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn submit_url_required_for_sequencer_rpc() {
        let toml = r#"
            mode = "sequencer"
            [ol]
            source = "rpc"
            client_url = "ws://strata:8432"
            [sequencer]
            [sequencer.prover]
            backend = "native"
            chunk_signing_key_path = "/tmp/chunk.key"
            acct_signing_key_path = "/tmp/acct.key"
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "bitcoind"
            [sequencer.da_fee_rate]
            fallback_policy_rate_wei_per_byte = 0
            refresh_interval_seconds = 60
            stale_after_seconds = 300
        "#;
        let err = AlpenClientConfig::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("ol_submit_url"));
    }

    /// The submission URL moved to `[sequencer]`, so a full node can't name
    /// it under `[ol]` any more. The unknown-field check is what says so.
    #[test]
    fn submit_url_under_ol_is_rejected() {
        let toml = r#"
            mode = "full_node"
            [ol]
            source = "rpc"
            client_url = "ws://strata:8432"
            submit_url = "http://strata:8433"
            [full_node]
            sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        "#;
        let err = AlpenClientConfig::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("ol.submit_url"), "{err}");
    }

    /// Zero blocktime would busy-loop the block builder, and a zero-capacity
    /// event channel panics inside `mpsc::channel`. Both fields are
    /// `NonZero`, so serde rejects the value and names the offending key.
    #[cfg(feature = "sequencer")]
    #[test]
    fn zero_is_rejected_for_nonzero_fields() {
        for (field, key) in [
            ("blocktime_ms = 0", "sequencer.blocktime_ms"),
            (
                "batch_event_channel_capacity = 0",
                "sequencer.batch_event_channel_capacity",
            ),
        ] {
            let toml = format!(
                r#"
                mode = "sequencer"
                [ol]
                source = "dummy"
                [sequencer]
                {field}
                [sequencer.prover]
                backend = "native"
                chunk_signing_key_path = "/tmp/chunk.key"
                acct_signing_key_path = "/tmp/acct.key"
                [sequencer.bitcoind]
                rpc_url = "http://bitcoind:18443"
                rpc_user = "user"
                rpc_password = "pass"
                network = "regtest"
                [sequencer.l1_fee_policy]
                fee_policy = "bitcoind"
                [sequencer.da_fee_rate]
                fallback_policy_rate_wei_per_byte = 0
                refresh_interval_seconds = 60
                stale_after_seconds = 300
            "#
            );
            let err = AlpenClientConfig::from_toml_str(&toml)
                .unwrap_err()
                .to_string();
            assert!(err.contains("nonzero"), "{field}: {err}");
            assert!(err.contains(key), "{field}: {err}");
        }
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn fixed_fee_rate_required_when_fee_policy_fixed() {
        let toml = r#"
            mode = "sequencer"
            [ol]
            source = "dummy"
            [sequencer]
            [sequencer.prover]
            backend = "native"
            chunk_signing_key_path = "/tmp/chunk.key"
            acct_signing_key_path = "/tmp/acct.key"
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "fixed"
            [sequencer.da_fee_rate]
            fallback_policy_rate_wei_per_byte = 0
            refresh_interval_seconds = 60
            stale_after_seconds = 300
        "#;
        let err = toml::from_str::<AlpenClientConfigFile>(toml).unwrap_err();
        assert!(err.to_string().contains("fixed_fee_rate") || err.to_string().contains("missing"));
    }

    #[cfg(not(feature = "sequencer"))]
    #[test]
    fn sequencer_mode_rejected_without_feature() {
        let err = AlpenClientConfig::from_toml_str(SEQUENCER_TOML).unwrap_err();
        assert!(err.to_string().contains("sequencer") && err.to_string().contains("feature"));
    }
}
