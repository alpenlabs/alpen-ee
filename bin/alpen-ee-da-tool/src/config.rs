//! Configuration for the EE DA reconstruction tool.

use std::{fs, num::NonZeroUsize, path::Path, str::FromStr};

use bitcoin::secp256k1::XOnlyPublicKey;
use eyre::Context;
use serde::{de::Error as _, Deserialize, Deserializer};
use strata_config::BitcoindConfig;

// Applied when the corresponding configuration setting is omitted.
const DEFAULT_FETCH_MAX_RETRIES: u16 = 3;
const DEFAULT_FETCH_RETRY_DELAY_MS: u64 = 1_000;
const DEFAULT_BLOCK_FETCH_CONCURRENCY: NonZeroUsize =
    NonZeroUsize::new(8).expect("8 is always NonZero");

fn default_block_fetch_concurrency() -> NonZeroUsize {
    DEFAULT_BLOCK_FETCH_CONCURRENCY
}

/// Operational configuration for EE DA reconstruction.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EeDaToolConfig {
    /// Minimum depth below the Bitcoin chain tip at which an L1 block is considered reorg-safe.
    l1_reorg_safe_depth: u32,

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

    /// URL of the OL RPC endpoint.
    ol_rpc_url: String,
}

impl EeDaToolConfig {
    /// Loads tool configuration from a TOML file.
    pub(crate) fn from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read tool config from {}", path.display()))?;
        toml::from_str(&contents)
            .wrap_err_with(|| format!("failed to decode tool config from {}", path.display()))
    }

    /// Returns the minimum depth at which an L1 block is considered reorg-safe.
    pub(crate) fn l1_reorg_safe_depth(&self) -> u32 {
        self.l1_reorg_safe_depth
    }

    /// Returns the public key of the EE sequencer that produces EE DA payloads.
    pub(crate) fn sequencer_pubkey(&self) -> XOnlyPublicKey {
        self.sequencer_pubkey
    }

    /// Returns the configured block fetch concurrency.
    pub(crate) fn block_fetch_concurrency(&self) -> NonZeroUsize {
        self.block_fetch_concurrency
    }

    /// Returns the Bitcoin RPC configuration.
    pub(crate) fn bitcoind(&self) -> &BitcoindConfig {
        &self.bitcoind
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

    /// Returns the URL of the OL RPC endpoint.
    pub(crate) fn ol_rpc_url(&self) -> &str {
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

    use super::*;

    #[test]
    fn test_missing_fetch_settings_use_defaults() {
        let config: EeDaToolConfig = toml::from_str(
            r#"
                l1_reorg_safe_depth = 6
                sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
                ol_rpc_url = "http://127.0.0.1:8432"

                [bitcoind]
                rpc_url = "http://127.0.0.1:18443"
                rpc_user = "rpcuser"
                rpc_password = "rpcpassword"
                network = "regtest"
            "#,
        )
        .expect("valid tool config");

        assert_eq!(config.l1_reorg_safe_depth(), 6);
        assert_eq!(config.fetch_max_retries(), DEFAULT_FETCH_MAX_RETRIES);
        assert_eq!(config.fetch_retry_delay_ms(), DEFAULT_FETCH_RETRY_DELAY_MS);
        assert_eq!(
            config.block_fetch_concurrency(),
            DEFAULT_BLOCK_FETCH_CONCURRENCY
        );
        assert_eq!(config.bitcoind().network, Network::Regtest);
        assert_eq!(config.ol_rpc_url(), "http://127.0.0.1:8432");
    }

    #[test]
    fn test_fetch_settings_override_defaults() {
        let config: EeDaToolConfig = toml::from_str(
            r#"
                l1_reorg_safe_depth = 6
                sequencer_pubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
                block_fetch_concurrency = 4
                ol_rpc_url = "http://127.0.0.1:8432"

                [bitcoind]
                rpc_url = "http://127.0.0.1:18443"
                rpc_user = "rpcuser"
                rpc_password = "rpcpassword"
                network = "regtest"
                retry_count = 7
                retry_interval = 250
            "#,
        )
        .expect("valid tool config");

        assert_eq!(config.fetch_max_retries(), 7);
        assert_eq!(config.fetch_retry_delay_ms(), 250);
        assert_eq!(config.block_fetch_concurrency().get(), 4);
    }
}
