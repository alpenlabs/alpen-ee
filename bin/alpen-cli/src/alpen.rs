use std::{
    error::Error,
    fmt::{self, Debug, Formatter},
    ops::{Deref, DerefMut},
};

use alloy::{
    eips::BlockNumberOrTag,
    network::EthereumWallet,
    primitives::B256,
    providers::{
        fillers::{
            BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
            WalletFiller,
        },
        Identity, Provider as AlloyProvider, ProviderBuilder, RootProvider,
    },
    transports::{
        http::reqwest::{
            header::{HeaderMap, HeaderValue, AUTHORIZATION},
            Client, Url,
        },
        Authorization,
    },
};
use bdk_wallet::bitcoin::Network;
use zeroize::ZeroizeOnDrop;

use crate::{
    constants::{MAINNET_ALPEN_CHAIN_ID, MAINNET_ALPEN_GENESIS_HASH},
    seed::Seed,
    settings::Settings,
};

// alloy moment 💀
type Provider = FillProvider<
    JoinFill<
        JoinFill<
            Identity,
            JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider,
>;

#[derive(Debug)]
pub struct AlpenWallet(Provider);

#[derive(ZeroizeOnDrop)]
pub struct AlpenRpcAuth {
    username: String,
    password: String,
}

impl AlpenRpcAuth {
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
}

impl Debug for AlpenRpcAuth {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AlpenRpcAuth")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

impl DerefMut for AlpenWallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for AlpenWallet {
    type Target = Provider;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

type BoxedError = Box<dyn Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum AlpenWalletError {
    #[error("invalid Alpen endpoint URL")]
    InvalidEndpoint(#[source] BoxedError),
    #[error("Alpen endpoint must use HTTP or HTTPS")]
    UnsupportedEndpointScheme,
    #[error("Alpen endpoint URL must not contain credentials; use the RPC auth settings")]
    CredentialsInEndpoint,
    #[error("Alpen RPC credentials require HTTPS")]
    InsecureAuthentication,
    #[error("failed to construct the Alpen RPC client")]
    HttpClient(#[source] BoxedError),
    #[error("failed to read Alpen network identity")]
    IdentityRequest(#[source] BoxedError),
    #[error("Alpen RPC did not return genesis block 0")]
    MissingGenesis,
    #[error("Alpen RPC chain ID mismatch: expected {expected}, received {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },
    #[error("Alpen RPC genesis mismatch: expected {expected}, received {actual}")]
    GenesisMismatch { expected: B256, actual: B256 },
}

impl AlpenWallet {
    pub async fn new(seed: &Seed, settings: &Settings) -> Result<Self, AlpenWalletError> {
        let endpoint: Url = settings
            .alpen_endpoint
            .parse()
            .map_err(|error| AlpenWalletError::InvalidEndpoint(Box::new(error)))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(AlpenWalletError::UnsupportedEndpointScheme);
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(AlpenWalletError::CredentialsInEndpoint);
        }
        if settings.alpen_rpc_auth.is_some() && endpoint.scheme() != "https" {
            return Err(AlpenWalletError::InsecureAuthentication);
        }

        let mut headers = HeaderMap::new();
        if let Some(auth) = &settings.alpen_rpc_auth {
            let mut value = HeaderValue::from_str(
                &Authorization::basic(&auth.username, &auth.password).to_string(),
            )
            .map_err(|error| AlpenWalletError::HttpClient(Box::new(error)))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|error| AlpenWalletError::HttpClient(Box::new(error)))?;
        let wallet = seed.get_alpen_wallet();

        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_reqwest(client, endpoint);
        if settings.network == Network::Bitcoin {
            let chain_id = provider
                .get_chain_id()
                .await
                .map_err(|error| AlpenWalletError::IdentityRequest(Box::new(error)))?;
            let genesis_hash = provider
                .get_block_by_number(BlockNumberOrTag::Number(0))
                .await
                .map_err(|error| AlpenWalletError::IdentityRequest(Box::new(error)))?
                .ok_or(AlpenWalletError::MissingGenesis)?
                .hash();
            validate_mainnet_identity(chain_id, genesis_hash)?;
        }

        Ok(Self(provider))
    }
}

fn validate_mainnet_identity(chain_id: u64, genesis_hash: B256) -> Result<(), AlpenWalletError> {
    if chain_id != MAINNET_ALPEN_CHAIN_ID {
        return Err(AlpenWalletError::ChainIdMismatch {
            expected: MAINNET_ALPEN_CHAIN_ID,
            actual: chain_id,
        });
    }
    if genesis_hash != MAINNET_ALPEN_GENESIS_HASH {
        return Err(AlpenWalletError::GenesisMismatch {
            expected: MAINNET_ALPEN_GENESIS_HASH,
            actual: genesis_hash,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_identity_rejects_wrong_chain() {
        assert!(validate_mainnet_identity(1, B256::ZERO).is_err());
    }
}
