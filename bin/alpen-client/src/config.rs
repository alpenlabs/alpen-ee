//! Typed configuration loaded from `--alpen-config <PATH>`.
//!
//! Two types with unambiguous responsibilities: [`AlpenClientConfigFile`] is
//! the public TOML file format (the only type that round-trips through
//! `toml`), [`AlpenClientConfig`] is the validated runtime representation the
//! rest of the binary uses. [`AlpenClientConfig::from_toml_str`] is the sole
//! entry point between the two; `AlpenClientConfigFile` never leaves this
//! module.

use alloy_primitives::{address, Address};
use alpen_ee_config::defaults::DEFAULT_DB_RETRY_COUNT;
use alpen_ee_ol_tracker::EpochTrackingMode;
use alpen_ee_params::AlpenParams;
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize};
use strata_config::{btcio::L1FeePolicyConfig, BitcoindConfig};
use strata_primitives::{buf::Buf32, L1Height};

// Mirrors bitcoind-async-client's upstream defaults, same as today's
// `bin/alpen-client/src/args.rs` (moved here since this is now where the
// `[sequencer.bitcoind]` defaults belong, but `BitcoindConfig` itself is
// defined upstream and doesn't apply its own defaults).
const DEFAULT_HEALTH_CHECK_HOST: &str = "0.0.0.0";
const DEFAULT_HEALTH_CHECK_PORT: u16 = 8080;
const DEFAULT_BENEFICIARY_ADDRESS: Address = address!("5400000000000000000000000000000000000010");
const DEFAULT_L1_REORG_SAFE_DEPTH: u32 = 6;
const DEFAULT_BATCH_SEALING_BLOCK_COUNT: u64 = 100;

// These two mirror constants defined behind the `sequencer` feature
// (`alpen_ee_sequencer::DEFAULT_BLOCKTIME_MS` and a private constant in
// `sequencer/mod.rs`) rather than referencing them directly: `SequencerConfig`
// has to compile in every build (it's an unconditional field on
// `AlpenClientConfigFile`, see below), but those crates/modules aren't linked
// in a slim (non-`sequencer`) build.
const DEFAULT_BLOCKTIME_MS: u64 = 5_000;
const DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY: usize = 64;

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

fn default_blocktime_ms() -> u64 {
    DEFAULT_BLOCKTIME_MS
}

fn default_batch_sealing_block_count() -> u64 {
    DEFAULT_BATCH_SEALING_BLOCK_COUNT
}

fn default_batch_event_channel_capacity() -> usize {
    DEFAULT_BATCH_EVENT_CHANNEL_CAPACITY
}

/// Rejects `0`; replaces `SequencerArgs::resolve_blocktime_ms`'s validation,
/// done inline during deserialize instead of as a separate resolve step.
fn positive_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(DeError::custom("must be greater than zero"));
    }
    Ok(value)
}

/// `Buf32`'s derived `Deserialize` goes through the `hex` crate's serde
/// helper, which (unlike `Buf32`'s own `FromStr`, used by the CLI/env-var
/// paths for `SEQUENCER_PRIVATE_KEY`/`--sequencer-pubkey` today) rejects an
/// `0x` prefix. Parse through `FromStr` instead so this field accepts the
/// same input shape as everywhere else a Buf32 is hand-typed by an operator.
fn buf32_from_hex<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Buf32, D::Error> {
    let s = String::deserialize(deserializer)?;
    s.parse::<Buf32>().map_err(DeError::custom)
}

/// The TOML file format. Private: [`AlpenClientConfig::from_toml_str`] is the
/// only place this type is constructed or consumed.
// Field order matters here, not just for readability: TOML requires every
// plain `key = value` pair to precede any `[table]` section, and serde's
// toml serializer emits fields in declaration order, so all scalar fields
// must come before `ol`/`full_node`/`sequencer` (each of which serializes as
// a table) — reordering them any other way fails to serialize at all
// (`ValueAfterTable`), caught by the round-trip tests below.
#[derive(Debug, Serialize, Deserialize)]
struct AlpenClientConfigFile {
    #[serde(default = "default_health_check_host")]
    health_check_host: String,
    #[serde(default = "default_health_check_port")]
    health_check_port: u16,
    // fixed constant default (alpen_ee_config::defaults::DEFAULT_DB_RETRY_COUNT, reused not
    // duplicated) — resolved here rather than left to AlpenEeConfig::new's own .unwrap_or,
    // since there's no reason for this layer to pass through an unresolved value.
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
        /// URL of the authenticated OL transaction submission RPC. Required
        /// (only) when `mode = "sequencer"` — checked in `AlpenClientConfig`'s
        /// `TryFrom` below, not here (spans two independent enums). The
        /// bearer token authenticating submission is deliberately not a
        /// field here at all — see `node.rs::resolve_ol_client`.
        submit_url: Option<String>,
    },
}

/// The resolved runtime representation — not "the" runtime config in the
/// `AlpenEeConfig` sense, just the primitives `AlpenEeConfig::new(...)` needs
/// (params comes from the separate `--alpen-params` flag) plus the binary's
/// own control flow. Plain struct, no Serde derive: `AlpenClientConfigFile`
/// is the only type that round-trips through TOML.
#[derive(Debug)]
pub(crate) struct AlpenClientConfig {
    pub(crate) health_check_host: String,
    pub(crate) health_check_port: u16,
    pub(crate) db_retry_count: u16,
    pub(crate) ol: OlConfig,
    pub(crate) l1_reorg_safe_depth: u32,
    pub(crate) genesis_l1_height: L1Height,
    pub(crate) mode: NodeMode,
}

#[derive(Debug)]
pub(crate) enum NodeMode {
    FullNode(FullNodeConfig),
    #[cfg(feature = "sequencer")]
    Sequencer(SequencerConfig),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FullNodeConfig {
    /// Full nodes can't derive the sequencer's pubkey themselves (they don't
    /// hold its private key) — operator-supplied network knowledge; contrast
    /// [`NodeMode::Sequencer`], which derives its own instead of being told
    /// it (see the note on `GossipConfig` construction in `node.rs`).
    #[serde(deserialize_with = "buf32_from_hex")]
    pub(crate) sequencer_pubkey: Buf32,
    /// Genuinely optional, not "required, currently unmodeled": full nodes
    /// get blocks purely via gossip (signed headers) + reth P2P sync — this
    /// URL is used for exactly one thing, forwarding user-submitted
    /// transactions to the sequencer's mempool
    /// (`crates/reth/node/src/node.rs`, `AlpenRethAddOnsBuilder::with_sequencer`).
    /// A full node without it is a valid read-only node (serves reads/sync,
    /// doesn't accept writes).
    pub(crate) sequencer_http_url: Option<String>,
}

// Exists at all only when the `sequencer` feature is compiled in — gated once, on the enum
// variant holding it (`NodeMode::Sequencer` above), not per-field or on this struct itself.
// The struct *definition* stays unconditional: `AlpenClientConfigFile.sequencer:
// Option<SequencerConfig>` has to be nameable in every build the same way `BitcoindConfig`/
// `L1FeePolicyConfig` do, and gating individual fields is backwards anyway — `main.rs`
// already hard-bails at startup on `mode = "sequencer"` in a slim build, so a slim build can
// parse-and-reject the `[sequencer]` table but never construct a live `NodeMode::Sequencer(_)`
// — the type existing latently, unused, costs nothing.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SequencerConfig {
    #[serde(default = "default_beneficiary_address")]
    pub(crate) beneficiary_address: Address,
    /// Replaces `SequencerArgs::resolve_blocktime_ms`; validated `> 0` inline
    /// during deserialize instead of as a separate resolve step.
    #[serde(default = "default_blocktime_ms", deserialize_with = "positive_u64")]
    pub(crate) blocktime_ms: u64,
    #[serde(default = "default_batch_sealing_block_count")]
    pub(crate) batch_sealing_block_count: u64,
    /// No serde default: defaults to `batch_sealing_block_count`, a sibling
    /// field, not a fixed constant — see the [`SequencerConfig::chunk_sealing_block_count`]
    /// accessor below.
    pub(crate) chunk_sealing_block_count: Option<u64>,
    /// Genuinely optional, not "has a default": `None` disables the
    /// gas-limit sealing policy entirely (block-count-only sealing) — no
    /// numeric default would mean that.
    pub(crate) chunk_sealing_gas_limit: Option<u64>,
    #[serde(default = "default_batch_event_channel_capacity")]
    pub(crate) batch_event_channel_capacity: usize,
    #[serde(default)]
    pub(crate) dev_native_prover: bool,
    /// Genuinely optional: only consulted when `dev_native_prover = false`
    /// (remote SP1 backend); `None` resolves to a built-in deadline inside
    /// the prover code that uses it, not eagerly here.
    pub(crate) sp1_proof_deadline_secs: Option<u64>,
    /// `[sequencer.bitcoind]` — reused verbatim from `strata_config`.
    pub(crate) bitcoind: BitcoindConfig,
    /// `[sequencer.l1_fee_policy]` — reused verbatim from `strata_config::btcio`.
    pub(crate) l1_fee_policy: L1FeePolicyConfig,
}

impl SequencerConfig {
    pub(crate) fn chunk_sealing_block_count(&self) -> u64 {
        self.chunk_sealing_block_count
            .unwrap_or(self.batch_sealing_block_count)
    }

    /// `--chunk-sealing-gas-limit` (now `chunk_sealing_gas_limit`) is
    /// validated against the genesis gas limit. EIP-1559 lets the per-block
    /// gas limit drift from genesis by ±1/1024 per block, so the actual
    /// block gas limit at runtime may be slightly higher than genesis. Uses
    /// 2× the genesis gas limit as a conservative floor to accommodate this
    /// drift while still catching obvious misconfigurations.
    ///
    /// Needs the `AlpenParams` artifact (loaded from the separate
    /// `--alpen-params` flag), so it can't be checked during TOML
    /// deserialization — called once from `node::launch` instead.
    pub(crate) fn validate_against_params(&self, params: &AlpenParams) -> eyre::Result<()> {
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
        let mode = match raw.mode {
            NodeModeTag::FullNode => {
                let fc = raw.full_node.ok_or_else(|| {
                    eyre::eyre!("[full_node] table required when mode = \"full_node\"")
                })?;
                NodeMode::FullNode(fc)
            }
            NodeModeTag::Sequencer => {
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
                    NodeMode::Sequencer(seq)
                }
            }
        };

        // The one cross-tree check no per-field attribute can express: `ol.submit_url` only
        // makes sense (and is only required) when this node both submits to OL (Sequencer)
        // and talks to a real OL RPC endpoint (Rpc, not Dummy) — two independent enums. In a
        // slim (non-`sequencer`) build `mode` can only ever be `FullNode`, so the check is
        // moot there — `NodeMode::Sequencer` isn't even nameable in that build.
        #[cfg(feature = "sequencer")]
        if let (NodeMode::Sequencer(_), OlSource::Rpc { submit_url: None, .. }) =
            (&mode, &raw.ol.source)
        {
            eyre::bail!(
                "ol.submit_url is required when mode = \"sequencer\" unless ol.source = \"dummy\""
            );
        }

        Ok(Self {
            health_check_host: raw.health_check_host,
            health_check_port: raw.health_check_port,
            db_retry_count: raw.db_retry_count,
            ol: raw.ol,
            l1_reorg_safe_depth: raw.l1_reorg_safe_depth,
            genesis_l1_height: raw.genesis_l1_height,
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
        let file: AlpenClientConfigFile = toml::from_str(contents)?;
        file.try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_NODE_TOML: &str = include_str!("../testdata/config.full_node.toml");
    const SEQUENCER_TOML: &str = include_str!("../testdata/config.sequencer.toml");

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

    #[cfg(feature = "sequencer")]
    #[test]
    fn sequencer_toml_resolves() {
        let config = AlpenClientConfig::from_toml_str(SEQUENCER_TOML).unwrap();
        let NodeMode::Sequencer(seq) = &config.mode else {
            panic!("expected sequencer mode");
        };
        assert_eq!(seq.blocktime_ms, 5_000);
        assert_eq!(seq.batch_sealing_block_count, 100);
        assert_eq!(seq.chunk_sealing_block_count(), 100);
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
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "bitcoind"
        "#;
        let err = AlpenClientConfig::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("submit_url"));
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn blocktime_ms_rejects_zero() {
        let toml = r#"
            mode = "sequencer"
            [ol]
            source = "dummy"
            [sequencer]
            blocktime_ms = 0
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "bitcoind"
        "#;
        let err = AlpenClientConfig::from_toml_str(toml).unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[cfg(feature = "sequencer")]
    #[test]
    fn fixed_fee_rate_required_when_fee_policy_fixed() {
        let toml = r#"
            mode = "sequencer"
            [ol]
            source = "dummy"
            [sequencer]
            [sequencer.bitcoind]
            rpc_url = "http://bitcoind:18443"
            rpc_user = "user"
            rpc_password = "pass"
            network = "regtest"
            [sequencer.l1_fee_policy]
            fee_policy = "fixed"
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
