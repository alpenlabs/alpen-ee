#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    env::var,
    fs::{create_dir_all, File, OpenOptions},
    io::{self, Read},
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, LazyLock},
};

use alloy::primitives::Address as AlpenAddress;
use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client};
use bdk_wallet::bitcoin::{Amount, Network, XOnlyPublicKey};
use config::{Config, ConfigError, File as ConfigFile, FileFormat};
use directories::ProjectDirs;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use shrex::Hex;
use strata_bridge_params::BridgeParams;
use strata_l1_txfmt::MagicBytes;
use terrors::OneOf;

use crate::{
    alpen::AlpenRpcAuth,
    constants::*,
    signet::{backend::SignetBackend, EsploraClient, SignetWallet},
};
#[cfg(feature = "test-mode")]
use crate::{constants::SEED_LEN, seed::Seed};

/// Environment variable overriding the project directories root.
const PROJ_DIRS_ENV: &str = "PROJ_DIRS";
/// Environment variable overriding the CLI config file path.
const CONFIG_FILE_ENV: &str = "CLI_CONFIG";
/// Default file name for the CLI config within the config directory.
const DEFAULT_CONFIG_FILENAME: &str = "config.toml";

/// Settings deserialized from the config file.
#[derive(Serialize, Deserialize)]
#[expect(missing_debug_implementations, reason = "contains RPC credentials")]
pub struct SettingsFromFile {
    /// Esplora server endpoint.
    pub esplora: Option<String>,
    /// Bitcoind RPC username.
    pub bitcoind_rpc_user: Option<String>,
    /// Bitcoind RPC password.
    pub bitcoind_rpc_pw: Option<String>,
    /// Path to the Bitcoind RPC cookie file.
    pub bitcoind_rpc_cookie: Option<PathBuf>,
    /// Bitcoind RPC endpoint.
    pub bitcoind_rpc_endpoint: Option<String>,
    /// Alpen network RPC endpoint.
    pub alpen_endpoint: String,
    /// Optional HTTP Basic credentials for the Alpen RPC endpoint.
    pub alpen_rpc_user: Option<String>,
    pub alpen_rpc_password: Option<String>,
    /// Faucet service endpoint. Mainnet has no faucet.
    pub faucet_endpoint: Option<String>,
    /// Mempool explorer endpoint.
    pub mempool_endpoint: Option<String>,
    /// Blockscout explorer endpoint.
    pub blockscout_endpoint: Option<String>,
    /// The aggregated Musig2 public key for the bridge.
    pub bridge_pubkey: Hex<[u8; 32]>,
    /// The address of the bridge precompile in alpen evm in hex.
    pub bridge_alpen_address: Option<String>,
    /// Fee to cover mining costs for the bridge to process deposits, in satoshis.
    pub bridge_fee_sats: Option<u64>,
    /// The number of confirmations to consider a Bitcoin transaction final.
    pub finality_depth: Option<u32>,
    /// L1 network the wallet operates on (e.g. "signet").
    ///
    /// Must match the network the ASM is anchored to.
    pub network: Network,
    /// SPS-50 magic bytes tagging protocol transactions on L1 (e.g. "ALPN").
    ///
    /// Must match the magic bytes in the ASM params.
    pub magic_bytes: MagicBytes,
    /// Bridge denomination in satoshis, used for both deposits and
    /// withdrawals.
    ///
    /// Must match the Bridge subprotocol denomination in the ASM params and the
    /// bridge denomination in the OL params, which are the same network value.
    pub bridge_denomination_sats: u64,
    /// Number of Bitcoin blocks after which the depositor can reclaim an
    /// unprocessed deposit request.
    ///
    /// Must match the Bridge subprotocol recovery delay in the ASM params.
    pub recovery_delay: u16,
    /// Maximum withdrawal amount in satoshis. Defaults to 1_000_000_000 (10 BTC).
    ///
    /// Withdrawals are batched in multiples of the denomination up to this cap, so
    /// it must match the OL params to avoid submitting amounts the OL STF rejects.
    pub max_withdrawal_amount_sats: Option<u64>,
    /// Maximum withdrawal BOSD descriptor length in bytes, including the type tag.
    ///
    /// Must match the OL params.
    pub max_withdrawal_descriptor_len: Option<u32>,
    /// Seed that can be passed directly for functional test.
    #[cfg(feature = "test-mode")]
    pub seed: Hex<[u8; SEED_LEN]>,
}

/// Settings struct filled with either config values or
/// opinionated defaults
#[derive(Debug)]
pub struct Settings {
    pub esplora: Option<String>,
    pub alpen_endpoint: String,
    pub alpen_rpc_auth: Option<AlpenRpcAuth>,
    pub data_dir: PathBuf,
    pub faucet_endpoint: Option<String>,
    pub bridge_musig2_pubkey: XOnlyPublicKey,
    pub descriptor_db: PathBuf,
    pub mempool_space_endpoint: Option<String>,
    pub blockscout_endpoint: Option<String>,
    pub bridge_alpen_address: AlpenAddress,
    pub linux_seed_file: PathBuf,
    pub config_file: PathBuf,
    pub signet_backend: Arc<dyn SignetBackend>,
    pub bridge_fee: Amount,
    pub finality_depth: u32,
    pub bridge_params: BridgeParams,
    /// L1 network the wallet operates on.
    pub network: Network,
    /// SPS-50 magic bytes tagging protocol transactions on L1.
    pub magic_bytes: MagicBytes,
    /// Deposit-request reclaim delay in Bitcoin blocks.
    pub recovery_delay: u16,
    #[cfg(feature = "test-mode")]
    pub seed: Seed,
}

pub static PROJ_DIRS: LazyLock<ProjectDirs> = LazyLock::new(|| match var(PROJ_DIRS_ENV).ok() {
    Some(path) => ProjectDirs::from_path(path.into()).expect("valid project path"),
    None => ProjectDirs::from("io", "alpenlabs", "alpen").expect("project dir should be available"),
});

pub static CONFIG_FILE: LazyLock<PathBuf> = LazyLock::new(|| match var(CONFIG_FILE_ENV).ok() {
    Some(path) => PathBuf::from_str(&path).expect("valid config path"),
    None => PROJ_DIRS
        .config_dir()
        .to_owned()
        .join(DEFAULT_CONFIG_FILENAME),
});

impl Settings {
    pub fn load() -> Result<Self, OneOf<(io::Error, config::ConfigError)>> {
        let proj_dirs = &PROJ_DIRS;
        let config_file = CONFIG_FILE.as_path();
        let linux_seed_file = proj_dirs.data_dir().to_owned().join("seed");

        create_dir_all(proj_dirs.config_dir()).map_err(OneOf::new)?;
        create_dir_all(proj_dirs.data_dir()).map_err(OneOf::new)?;

        let mut config_handle = open_file(config_file).map_err(OneOf::new)?;
        ensure_owner_only(&config_handle, config_file).map_err(OneOf::new)?;
        let mut config_contents = String::new();
        config_handle
            .read_to_string(&mut config_contents)
            .map_err(OneOf::new)?;
        let from_file: SettingsFromFile = Config::builder()
            .add_source(ConfigFile::from_str(&config_contents, FileFormat::Toml))
            .build()
            .map_err(OneOf::new)?
            .try_deserialize::<SettingsFromFile>()
            .map_err(OneOf::new)?;

        if from_file.network == Network::Bitcoin {
            validate_mainnet_endpoint("Alpen RPC", &from_file.alpen_endpoint)
                .map_err(OneOf::new)?;
            if let Some(endpoint) = from_file
                .esplora
                .as_deref()
                .or(from_file.bitcoind_rpc_endpoint.as_deref())
            {
                validate_mainnet_endpoint("Bitcoin backend", endpoint).map_err(OneOf::new)?;
            }
        }

        let sync_backend: Arc<dyn SignetBackend> = match (
            from_file.esplora.clone(),
            from_file.bitcoind_rpc_user,
            from_file.bitcoind_rpc_pw,
            from_file.bitcoind_rpc_cookie,
            from_file.bitcoind_rpc_endpoint,
        ) {
            (Some(url), None, None, None, None) => {
                Arc::new(EsploraClient::new(&url).map_err(|error| {
                    OneOf::new(ConfigError::Message(format!(
                        "invalid esplora URL: {error}"
                    )))
                })?)
            }
            (None, Some(user), Some(password), None, Some(url)) => Arc::new(Arc::new(
                Client::new(&url, Auth::UserPass(user, password)).map_err(|error| {
                    OneOf::new(ConfigError::Message(format!(
                        "invalid Bitcoin Core RPC configuration: {error}"
                    )))
                })?,
            )),
            (None, None, None, Some(cookie_file), Some(url)) => Arc::new(Arc::new(
                Client::new(&url, Auth::CookieFile(cookie_file)).map_err(|error| {
                    OneOf::new(ConfigError::Message(format!(
                        "invalid Bitcoin Core RPC configuration: {error}"
                    )))
                })?,
            )),
            _ => {
                return Err(OneOf::new(ConfigError::Message(
                    "configure exactly one Bitcoin backend".into(),
                )))
            }
        };

        // These fields are hand-merged into config.toml by operators, so a bad
        // value must surface as a config error, not a panic.
        let bridge_musig2_pubkey =
            XOnlyPublicKey::from_slice(&from_file.bridge_pubkey.0).map_err(|e| {
                OneOf::new(ConfigError::Message(format!(
                    "bridge_pubkey is not a valid x-only public key: {e}"
                )))
            })?;
        let max_withdrawal_amount = match from_file.network {
            Network::Bitcoin => from_file.max_withdrawal_amount_sats,
            _ => from_file
                .max_withdrawal_amount_sats
                .or(Some(DEFAULT_MAX_WITHDRAWAL_SATS)),
        };
        let bridge_params = BridgeParams::new_with_descriptor_limit(
            from_file.bridge_denomination_sats,
            max_withdrawal_amount,
            from_file
                .max_withdrawal_descriptor_len
                .unwrap_or(DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN),
        )
        .map_err(|e| {
            OneOf::new(ConfigError::Message(format!(
                "invalid withdrawal params in config: {e}"
            )))
        })?;
        let bridge_alpen_address = AlpenAddress::from_str(
            from_file
                .bridge_alpen_address
                .as_deref()
                .unwrap_or(DEFAULT_BRIDGE_ALPEN_ADDRESS),
        )
        .map_err(|error| {
            OneOf::new(ConfigError::Message(format!(
                "bridge_alpen_address is not a valid EVM address: {error}"
            )))
        })?;
        let bridge_fee = from_file
            .bridge_fee_sats
            .map(Amount::from_sat)
            .unwrap_or(DEFAULT_BRIDGE_FEE);
        let finality_depth = from_file.finality_depth.unwrap_or(DEFAULT_FINALITY_DEPTH);
        validate_mainnet_profile(
            (
                from_file.network,
                from_file.magic_bytes,
                from_file.recovery_delay,
            ),
            bridge_musig2_pubkey,
            bridge_alpen_address,
            &bridge_params,
            bridge_fee,
            finality_depth,
        )
        .map_err(OneOf::new)?;

        let alpen_rpc_auth = match (from_file.alpen_rpc_user, from_file.alpen_rpc_password) {
            (None, None) => None,
            (Some(username), Some(password)) => Some(AlpenRpcAuth::new(username, password)),
            _ => {
                return Err(OneOf::new(ConfigError::Message(
                    "alpen_rpc_user and alpen_rpc_password must be configured together".into(),
                )))
            }
        };
        let descriptor_file = proj_dirs.data_dir().join(match from_file.network {
            Network::Bitcoin => "descriptors-bitcoin",
            _ => "descriptors",
        });
        if from_file.network == Network::Bitcoin {
            #[cfg(target_os = "linux")]
            open_owner_only(&linux_seed_file).map_err(OneOf::new)?;
            open_owner_only(&SignetWallet::db_path(
                "default",
                proj_dirs.data_dir(),
                from_file.network,
            ))
            .map_err(OneOf::new)?;
        }

        Ok(Settings {
            esplora: from_file.esplora,
            alpen_endpoint: from_file.alpen_endpoint,
            alpen_rpc_auth,
            data_dir: proj_dirs.data_dir().to_owned(),
            faucet_endpoint: from_file.faucet_endpoint,
            bridge_musig2_pubkey,
            descriptor_db: descriptor_file,
            mempool_space_endpoint: from_file.mempool_endpoint,
            blockscout_endpoint: from_file.blockscout_endpoint,
            bridge_alpen_address,
            linux_seed_file,
            config_file: CONFIG_FILE.clone(),
            signet_backend: sync_backend,
            bridge_fee,
            finality_depth,
            bridge_params,
            network: from_file.network,
            magic_bytes: from_file.magic_bytes,
            recovery_delay: from_file.recovery_delay,
            #[cfg(feature = "test-mode")]
            seed: Seed::from_file(from_file.seed),
        })
    }
}

fn validate_mainnet_endpoint(name: &str, endpoint: &str) -> Result<(), ConfigError> {
    let url = Url::parse(endpoint)
        .map_err(|error| ConfigError::Message(format!("invalid {name} URL: {error}")))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() == "https" || url.scheme() == "http" && loopback {
        Ok(())
    } else {
        Err(ConfigError::Message(format!(
            "mainnet {name} must use HTTPS unless it is a loopback endpoint"
        )))
    }
}

fn validate_mainnet_profile(
    network_fields: (Network, MagicBytes, u16),
    bridge_pubkey: XOnlyPublicKey,
    bridge_address: AlpenAddress,
    bridge_params: &BridgeParams,
    bridge_fee: Amount,
    finality_depth: u32,
) -> Result<(), ConfigError> {
    let (network, magic_bytes, recovery_delay) = network_fields;
    if network != Network::Bitcoin {
        return Ok(());
    }

    let matches_deployment = magic_bytes == MAINNET_MAGIC_BYTES
        && bridge_pubkey.serialize() == MAINNET_BRIDGE_PUBKEY
        && bridge_address == MAINNET_BRIDGE_ALPEN_ADDRESS
        && bridge_params.denomination() == MAINNET_BRIDGE_DENOMINATION_SATS
        && bridge_fee.to_sat() == MAINNET_BRIDGE_FEE_SATS
        && recovery_delay == MAINNET_RECOVERY_DELAY
        && bridge_params.max_withdrawal_amount().is_none()
        && bridge_params.max_withdrawal_descriptor_len() == DEFAULT_MAX_WITHDRAWAL_DESCRIPTOR_LEN
        && finality_depth == DEFAULT_FINALITY_DEPTH;
    if matches_deployment {
        Ok(())
    } else {
        Err(ConfigError::Message(
            "mainnet settings do not match the deployed network profile".into(),
        ))
    }
}

fn open_owner_only(path: &Path) -> io::Result<File> {
    let file = open_file(path)?;
    ensure_owner_only(&file, path)?;
    Ok(file)
}

fn open_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn ensure_owner_only(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(not(unix))]
    let _ = (file, path);
    #[cfg(unix)]
    {
        let mode = file.metadata()?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} has insecure permissions {mode:#o}; expected 0600",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use toml;

    use super::*;

    #[cfg(not(feature = "test-mode"))]
    #[test]
    fn parses_deployed_mainnet_profile() {
        let parsed: SettingsFromFile =
            toml::from_str(include_str!("../configs/mainnet.toml.example")).unwrap();
        assert_eq!(parsed.network, Network::Bitcoin);
        assert_eq!(parsed.magic_bytes, MAINNET_MAGIC_BYTES);
        assert_eq!(parsed.bridge_fee_sats, Some(MAINNET_BRIDGE_FEE_SATS));
    }

    #[test]
    fn mainnet_endpoints_require_https_or_loopback() {
        assert!(validate_mainnet_endpoint("test", "https://example.com").is_ok());
        assert!(validate_mainnet_endpoint("test", "http://127.0.0.1:8332").is_ok());
        assert!(validate_mainnet_endpoint("test", "http://example.com").is_err());
    }

    #[test]
    fn test_parses_datatool_network_profile_snippet() {
        // Verbatim output of `strata-datatool gen-asm-params --cli-config`.
        // Must stay byte-identical to the literal pinned by
        // `cli_network_profile_matches_cli_config_schema` in bin/datatool, so
        // a field rename on either side fails one of the two tests.
        let snippet = "# Alpen CLI network profile derived from the ASM params.\n\
             # Merge these fields into the CLI's config.toml.\n\
             network = \"signet\"\n\
             magic_bytes = \"ALPN\"\n\
             bridge_pubkey = \"14ebfa9a90fee3020686b5334b297b675a9f29282f44b6c3a4ab1f0582021839\"\n\
             bridge_denomination_sats = 100000000\n\
             recovery_delay = 1008\n\
             max_withdrawal_amount_sats = 1000000000\n\
             max_withdrawal_descriptor_len = 81\n";

        let config = format!(
            "{snippet}\n\
             alpen_endpoint = \"https://rpc.testnet.alpenlabs.io\"\n\
             faucet_endpoint = \"https://faucet-api.testnet.alpenlabs.io\"\n\
             seed = \"000102030405060708090a0b0c0d0e0f\"\n"
        );

        // Deserialized through the `config` crate, not `toml`, because that is
        // the path `Settings::load` actually takes: the crate round-trips values
        // through its own `Value` layer, which can diverge from direct TOML
        // deserialization for custom visitors.
        let parsed: SettingsFromFile = Config::builder()
            .add_source(config::File::from_str(&config, config::FileFormat::Toml))
            .build()
            .expect("generated snippet should build as CLI config")
            .try_deserialize()
            .expect("generated snippet should parse as CLI config");

        assert_eq!(parsed.network, Network::Signet);
        assert_eq!(parsed.magic_bytes, MagicBytes::new(*b"ALPN"));
        assert_eq!(parsed.bridge_denomination_sats, 100_000_000);
        assert_eq!(parsed.recovery_delay, 1_008);
        assert_eq!(parsed.max_withdrawal_amount_sats, Some(1_000_000_000));
        assert_eq!(parsed.max_withdrawal_descriptor_len, Some(81));
    }

    #[test]
    fn test_settings_from_file_serde_roundtrip() {
        let config = r#"
            esplora = "https://esplora.testnet.alpenlabs.io"
            bitcoind_rpc_user = "user"
            bitcoind_rpc_pw = "pass"
            bitcoind_rpc_endpoint = "http://127.0.0.1:38332"
            alpen_endpoint = "https://rpc.testnet.alpenlabs.io"
            faucet_endpoint = "https://faucet-api.testnet.alpenlabs.io"
            mempool_endpoint = "https://bitcoin.testnet.alpenlabs.io"
            blockscout_endpoint = "https://explorer.testnet.alpenlabs.io"
            bridge_pubkey = "1d3e9c0417ba7d3551df5a1cc1dbe227aa4ce89161762454d92bfc2b1d5886f7"
            network = "signet"
            magic_bytes = "ALPN"
            bridge_denomination_sats = 100_000_000
            recovery_delay = 1008
            max_withdrawal_descriptor_len = 81
            seed = "000102030405060708090a0b0c0d0e0f"
        "#;

        // Deserialize from TOML string
        let parsed: SettingsFromFile =
            toml::from_str(config).expect("failed to parse SettingsFromFile from TOML");

        // Serialize back to TOML string
        let serialized =
            toml::to_string(&parsed).expect("failed to serialize SettingsFromFile to TOML");

        // Deserialize again
        let reparsed: SettingsFromFile =
            toml::from_str(&serialized).expect("failed to deserialize serialized SettingsFromFile");

        // Assert important fields survived round-trip
        assert_eq!(parsed.esplora, reparsed.esplora);
        assert_eq!(parsed.alpen_endpoint, reparsed.alpen_endpoint);
        assert_eq!(parsed.faucet_endpoint, reparsed.faucet_endpoint);
        assert_eq!(parsed.bridge_pubkey.0, reparsed.bridge_pubkey.0);
        assert_eq!(parsed.network, reparsed.network);
        assert_eq!(parsed.magic_bytes, reparsed.magic_bytes);
        assert_eq!(
            parsed.bridge_denomination_sats,
            reparsed.bridge_denomination_sats
        );
        assert_eq!(parsed.recovery_delay, reparsed.recovery_delay);
        assert_eq!(
            parsed.max_withdrawal_descriptor_len,
            reparsed.max_withdrawal_descriptor_len
        );
    }
}
