//! Configuration for the EE DA verifier.

use std::{
    fs,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    path::Path,
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use bitcoin::{secp256k1::XOnlyPublicKey, Network};
use serde::{de::Error as _, Deserialize, Deserializer};
use url::Url;

// Applied when the corresponding configuration setting is omitted.
const DEFAULT_FETCH_MAX_RETRIES: u16 = 3;
const DEFAULT_FETCH_RETRY_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_L1_SCAN_WINDOW_SIZE: NonZeroU32 =
    NonZeroU32::new(500).expect("500 is always NonZero");
const DEFAULT_BLOCK_FETCH_CONCURRENCY: NonZeroUsize =
    NonZeroUsize::new(8).expect("8 is always NonZero");

fn default_max_l1_scan_window_size() -> NonZeroU32 {
    DEFAULT_MAX_L1_SCAN_WINDOW_SIZE
}

fn default_block_fetch_concurrency() -> NonZeroUsize {
    DEFAULT_BLOCK_FETCH_CONCURRENCY
}

/// Non-secret Bitcoin RPC connection and retry configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BitcoindConfig {
    rpc_url: String,
    network: Network,
    retry_count: Option<u16>,
    retry_interval: Option<u64>,
}

impl BitcoindConfig {
    /// Returns the Bitcoin RPC endpoint URL.
    pub(crate) fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
}

/// Operational configuration for EE DA reconstruction and verification.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaVerifierConfig {
    /// Minimum depth below the Bitcoin chain tip at which an L1 block is considered reorg-safe.
    ///
    /// Zero treats the current tip as reorg-safe.
    l1_reorg_safe_depth: u32,

    /// Interval between periodic EE DA recovery and state verification runs, in milliseconds.
    verification_interval_ms: NonZeroU64,

    /// Maximum number of L1 blocks scanned for DA in one recovery window.
    #[serde(default = "default_max_l1_scan_window_size")]
    max_l1_scan_window_size: NonZeroU32,

    /// Public key of the EE sequencer that produces EE DA payloads.
    #[serde(deserialize_with = "deserialize_xonly_public_key")]
    sequencer_pubkey: XOnlyPublicKey,

    /// Maximum number of Bitcoin blocks fetched concurrently.
    ///
    /// Each slot may retain one decoded block in memory. Defaults to
    /// [`DEFAULT_BLOCK_FETCH_CONCURRENCY`].
    #[serde(default = "default_block_fetch_concurrency")]
    block_fetch_concurrency: NonZeroUsize,

    /// Bitcoin RPC connection and retry configuration.
    bitcoind: BitcoindConfig,

    /// URL of an OL sequencer RPC endpoint that retains EE account update manifests and
    /// inner-state roots.
    ol_rpc_url: Url,
}

impl DaVerifierConfig {
    /// Loads verifier configuration from a TOML file.
    pub(crate) fn from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read verifier config from {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to decode verifier config from {}", path.display()))
    }

    /// Returns the minimum depth at which an L1 block is considered reorg-safe.
    pub(crate) fn l1_reorg_safe_depth(&self) -> u32 {
        self.l1_reorg_safe_depth
    }

    /// Returns the interval between periodic EE DA recovery and state verification runs.
    pub(crate) fn verification_interval(&self) -> Duration {
        Duration::from_millis(self.verification_interval_ms.get())
    }

    /// Returns the maximum number of L1 blocks scanned for DA in one recovery window.
    pub(crate) fn max_l1_scan_window_size(&self) -> NonZeroU32 {
        self.max_l1_scan_window_size
    }

    /// Returns the public key of the EE sequencer that produces EE DA payloads.
    pub(crate) fn sequencer_pubkey(&self) -> XOnlyPublicKey {
        self.sequencer_pubkey
    }

    /// Returns the Bitcoin RPC configuration.
    pub(crate) fn bitcoind(&self) -> &BitcoindConfig {
        &self.bitcoind
    }

    /// Returns the expected Bitcoin network.
    pub(crate) fn bitcoin_network(&self) -> Network {
        self.bitcoind.network
    }

    /// Returns the maximum number of retries for each Bitcoin block fetch.
    pub(crate) fn fetch_max_retries(&self) -> u16 {
        self.bitcoind
            .retry_count
            .unwrap_or(DEFAULT_FETCH_MAX_RETRIES)
    }

    /// Returns the initial delay between Bitcoin block fetch retries.
    pub(crate) fn fetch_retry_delay_ms(&self) -> u64 {
        self.bitcoind
            .retry_interval
            .unwrap_or(DEFAULT_FETCH_RETRY_DELAY_MS)
    }

    /// Returns the configured block fetch concurrency.
    pub(crate) fn block_fetch_concurrency(&self) -> NonZeroUsize {
        self.block_fetch_concurrency
    }

    /// Returns the URL of the OL RPC endpoint.
    pub(crate) fn ol_rpc_url(&self) -> &Url {
        &self.ol_rpc_url
    }
}

fn deserialize_xonly_public_key<'de, D>(deserializer: D) -> Result<XOnlyPublicKey, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = String::deserialize(deserializer)?;
    XOnlyPublicKey::from_str(&encoded).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use bitcoin::Network;
    use toml::{de::Error as TomlError, value::Table, Value};

    use super::*;

    /// Minimal configuration that parses, used as the base for override tests.
    const BASE_CONFIG: &str = r#"
        l1_reorg_safe_depth = 6
        verification_interval_ms = 1000
        sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        ol_rpc_url = "http://127.0.0.1:8432"

        [bitcoind]
        rpc_url = "http://127.0.0.1:18443"
        network = "regtest"
    "#;

    /// Returns [`BASE_CONFIG`] with each `key` set to its `value`.
    ///
    /// A dotted key sets a field of that sub-table, as in TOML itself.
    fn base_config_with<'a>(overrides: impl IntoIterator<Item = (&'a str, Value)>) -> Table {
        let mut table: Table = toml::from_str(BASE_CONFIG).expect("base config parses");
        for (key, value) in overrides {
            match key.split_once('.') {
                Some((parent, field)) => {
                    let Some(Value::Table(nested)) = table.get_mut(parent) else {
                        panic!("base config has no `{parent}` table");
                    };
                    nested.insert(field.to_owned(), value);
                }
                None => {
                    table.insert(key.to_owned(), value);
                }
            }
        }
        table
    }

    /// Deserializes a configuration table.
    fn parse_config(table: Table) -> Result<DaVerifierConfig, TomlError> {
        Value::Table(table).try_into()
    }

    #[test]
    fn test_missing_fetch_settings_use_defaults() {
        let config: DaVerifierConfig = toml::from_str(BASE_CONFIG).expect("valid verifier config");

        assert_eq!(config.l1_reorg_safe_depth(), 6);
        assert_eq!(config.verification_interval(), Duration::from_secs(1));
        assert_eq!(
            config.max_l1_scan_window_size(),
            DEFAULT_MAX_L1_SCAN_WINDOW_SIZE
        );
        assert_eq!(config.fetch_max_retries(), DEFAULT_FETCH_MAX_RETRIES);
        assert_eq!(config.fetch_retry_delay_ms(), DEFAULT_FETCH_RETRY_DELAY_MS);
        assert_eq!(
            config.block_fetch_concurrency(),
            DEFAULT_BLOCK_FETCH_CONCURRENCY
        );
        assert_eq!(config.bitcoin_network(), Network::Regtest);
        assert_eq!(config.ol_rpc_url().as_str(), "http://127.0.0.1:8432/");
    }

    #[test]
    fn test_fetch_settings_override_defaults() {
        let config = parse_config(base_config_with([
            ("verification_interval_ms", Value::Integer(250)),
            ("max_l1_scan_window_size", Value::Integer(25)),
            ("block_fetch_concurrency", Value::Integer(4)),
            ("bitcoind.retry_count", Value::Integer(7)),
            ("bitcoind.retry_interval", Value::Integer(250)),
        ]))
        .expect("valid verifier config");

        assert_eq!(config.fetch_max_retries(), 7);
        assert_eq!(config.fetch_retry_delay_ms(), 250);
        assert_eq!(config.block_fetch_concurrency().get(), 4);
        assert_eq!(config.verification_interval(), Duration::from_millis(250));
        assert_eq!(config.max_l1_scan_window_size().get(), 25);
    }

    #[test]
    fn test_zero_verification_interval_is_rejected() {
        // The accepted value proves the key is recognised, so the rejection
        // below cannot be an unknown-field error after a rename.
        let accepted = base_config_with([("verification_interval_ms", Value::Integer(250))]);
        let rejected = base_config_with([("verification_interval_ms", Value::Integer(0))]);

        assert!(parse_config(accepted).is_ok());
        assert!(parse_config(rejected).is_err());
    }

    #[test]
    fn test_zero_l1_scan_window_size_is_rejected() {
        // The accepted value proves the key is recognised, so the rejection
        // below cannot be an unknown-field error after a rename.
        let accepted = base_config_with([("max_l1_scan_window_size", Value::Integer(25))]);
        let rejected = base_config_with([("max_l1_scan_window_size", Value::Integer(0))]);

        assert!(parse_config(accepted).is_ok());
        assert!(parse_config(rejected).is_err());
    }

    #[test]
    fn test_plaintext_bitcoind_credentials_are_rejected() {
        let config = base_config_with([
            ("bitcoind.rpc_user", Value::String("rpcuser".to_owned())),
            (
                "bitcoind.rpc_password",
                Value::String("rpcpassword".to_owned()),
            ),
        ]);

        assert!(parse_config(config).is_err());
    }

    #[test]
    fn test_invalid_ol_rpc_url_rejected() {
        let accepted = base_config_with([(
            "ol_rpc_url",
            Value::String("http://127.0.0.1:8432".to_owned()),
        )]);
        let rejected = base_config_with([("ol_rpc_url", Value::String("not a URL".to_owned()))]);

        assert!(parse_config(accepted).is_ok());
        assert!(parse_config(rejected).is_err());
    }
}
