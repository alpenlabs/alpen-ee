//! Command-line arguments for the EE DA verifier.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use alpen_params::AlpenParams;
use anyhow::Context;
use clap::Parser;
use strata_identifiers::L1Height;

const BITCOIND_RPC_USER_ENV: &str = "BITCOIND_RPC_USER";
const BITCOIND_RPC_PASSWORD_ENV: &str = "BITCOIND_RPC_PASSWORD";

/// Credentials loaded from the verifier process environment for Bitcoin RPC authentication.
pub(crate) struct BitcoinRpcCredentials {
    user: String,
    password: String,
}

impl BitcoinRpcCredentials {
    fn new(user: String, password: String) -> Self {
        Self { user, password }
    }

    /// Returns the credential parts for constructing the Bitcoin RPC client.
    pub(crate) fn into_parts(self) -> (String, String) {
        (self.user, self.password)
    }
}

/// Runs the EE DA verifier service.
#[derive(Parser)]
pub(crate) struct Args {
    /// Path to the JSON-serialized Alpen params artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) alpen_params: PathBuf,

    /// Path to the verifier's TOML configuration file.
    #[arg(long, value_name = "PATH")]
    pub(crate) config: PathBuf,

    /// Directory containing the database(s) used by the verifier.
    #[arg(long, value_name = "PATH")]
    pub(crate) datadir: PathBuf,

    /// Path to the reconstruction snapshot file.
    #[arg(long, value_name = "PATH")]
    pub(crate) snapshot: PathBuf,

    /// L1 height from which reconstruction starts when no snapshot is available.
    #[arg(long)]
    pub(crate) genesis_l1_height: L1Height,
}

pub(crate) fn load_alpen_params(path: impl AsRef<Path>) -> anyhow::Result<AlpenParams> {
    let path = path.as_ref();
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read Alpen params from {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode Alpen params from {}", path.display()))
}

/// Reads the Bitcoin RPC credentials required by the verifier.
pub(crate) fn load_bitcoind_rpc_credentials_from_env() -> anyhow::Result<BitcoinRpcCredentials> {
    load_bitcoind_rpc_credentials(|name| env::var(name).ok())
}

fn load_bitcoind_rpc_credentials(
    mut read_var: impl FnMut(&str) -> Option<String>,
) -> anyhow::Result<BitcoinRpcCredentials> {
    let rpc_user = read_required_env_var(BITCOIND_RPC_USER_ENV, &mut read_var)?;
    let rpc_password = read_required_env_var(BITCOIND_RPC_PASSWORD_ENV, &mut read_var)?;
    Ok(BitcoinRpcCredentials::new(rpc_user, rpc_password))
}

fn read_required_env_var(
    name: &'static str,
    read_var: &mut impl FnMut(&str) -> Option<String>,
) -> anyhow::Result<String> {
    match read_var(name) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => anyhow::bail!("{name} environment variable is required"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitcoind_rpc_credentials_are_read_from_environment() {
        let credentials = load_bitcoind_rpc_credentials(|name| match name {
            BITCOIND_RPC_USER_ENV => Some("rpc-user".to_owned()),
            BITCOIND_RPC_PASSWORD_ENV => Some("rpc-password".to_owned()),
            _ => None,
        })
        .expect("both Bitcoin RPC credentials are available");

        assert_eq!(credentials.user, "rpc-user");
        assert_eq!(credentials.password, "rpc-password");
    }

    #[test]
    fn test_missing_bitcoind_rpc_user_is_rejected() {
        let result = load_bitcoind_rpc_credentials(|name| {
            (name == BITCOIND_RPC_PASSWORD_ENV).then(|| "rpc-password".to_owned())
        });
        let Err(error) = result else {
            panic!("missing Bitcoin RPC user must fail");
        };

        assert!(error.to_string().contains(BITCOIND_RPC_USER_ENV));
    }

    #[test]
    fn test_missing_bitcoind_rpc_password_is_rejected() {
        let result = load_bitcoind_rpc_credentials(|name| {
            (name == BITCOIND_RPC_USER_ENV).then(|| "rpc-user".to_owned())
        });
        let Err(error) = result else {
            panic!("missing Bitcoin RPC password must fail");
        };

        assert!(error.to_string().contains(BITCOIND_RPC_PASSWORD_ENV));
    }
}
