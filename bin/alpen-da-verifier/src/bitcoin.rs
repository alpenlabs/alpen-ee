//! Bitcoin RPC helpers for the EE DA verifier.

use std::future::Future;

use anyhow::Context;
use bitcoin::Network;
use bitcoind_async_client::{
    client::Auth, error::ClientError, traits::Reader, Client, ClientResult,
};
use strata_identifiers::L1Height;
use thiserror::Error;

use crate::{args::BitcoinRpcCredentials, config::BitcoindConfig};

/// Bitcoind RPC error code returned while the node is warming up.
const BITCOIND_RPC_WARMUP: i32 = -28;

/// Failure to fetch the current Bitcoin chain tip.
#[derive(Debug, Error)]
pub(crate) enum FetchBitcoinTipError {
    /// The Bitcoin RPC request failed.
    #[error("failed to fetch Bitcoin tip: {0}")]
    Rpc(#[source] ClientError),

    /// The returned height does not fit the supported L1 height type.
    #[error("Bitcoin tip {tip_height} exceeds the supported L1 height range")]
    HeightOutOfRange { tip_height: u64 },
}

impl FetchBitcoinTipError {
    /// Returns whether a later verification cycle may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::Rpc(source) => {
                // Startup fails fast and relies on pod restart. Once running, retrying the next
                // cycle preserves the in-memory scan cursor and commit/reveal parser state.
                is_recoverable_rpc_error(source)
            }
            Self::HeightOutOfRange { .. } => false,
        }
    }
}

/// Failure to read the Bitcoin RPC network or match it against the configured network.
#[derive(Debug, Error)]
pub(crate) enum BitcoinNetworkCheckError {
    /// The Bitcoin RPC request failed.
    #[error("failed to fetch Bitcoin network: {0}")]
    Rpc(#[source] ClientError),

    /// The RPC server belongs to a different Bitcoin network.
    #[error("configured Bitcoin network {expected} does not match RPC network {actual}")]
    NetworkMismatch { expected: Network, actual: Network },
}

/// Creates a Bitcoin RPC client from the supplied configuration.
pub(crate) fn create_bitcoin_rpc_client(
    config: &BitcoindConfig,
    credentials: BitcoinRpcCredentials,
) -> anyhow::Result<Client> {
    // Verifier operations own retry policy so one failed request does not pass through two nested
    // retry loops.
    let (rpc_user, rpc_password) = credentials.into_parts();
    Client::new(
        config.rpc_url().to_owned(),
        Auth::UserPass(rpc_user, rpc_password),
        Some(0),
        None,
        None,
    )
    .context("failed to create Bitcoin RPC client")
}

/// Provides the Bitcoin network reported by an RPC endpoint.
pub(crate) trait BitcoinNetworkReader {
    /// Fetches the network served by the Bitcoin RPC endpoint.
    fn network(&self) -> impl Future<Output = ClientResult<Network>> + Send;
}

impl<T> BitcoinNetworkReader for T
where
    T: Reader + Sync,
{
    fn network(&self) -> impl Future<Output = ClientResult<Network>> + Send {
        Reader::network(self)
    }
}

/// Ensures the Bitcoin RPC server uses the expected network.
pub(crate) async fn ensure_bitcoin_network<R>(
    client: &R,
    expected_network: Network,
) -> Result<(), BitcoinNetworkCheckError>
where
    R: BitcoinNetworkReader + Sync,
{
    let actual_network = client
        .network()
        .await
        .map_err(BitcoinNetworkCheckError::Rpc)?;
    if actual_network != expected_network {
        return Err(BitcoinNetworkCheckError::NetworkMismatch {
            expected: expected_network,
            actual: actual_network,
        });
    }
    Ok(())
}

/// Fetches the current Bitcoin chain tip height.
pub(crate) async fn fetch_bitcoin_tip_height(
    client: &Client,
) -> Result<L1Height, FetchBitcoinTipError> {
    let tip_height = client
        .get_block_count()
        .await
        .map_err(FetchBitcoinTipError::Rpc)?;
    L1Height::try_from(tip_height)
        .map_err(|_| FetchBitcoinTipError::HeightOutOfRange { tip_height })
}

fn is_recoverable_rpc_error(error: &ClientError) -> bool {
    error.is_retriable() || matches!(error, ClientError::Server(BITCOIND_RPC_WARMUP, _))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct MockNetworkReader {
        response: Mutex<Option<ClientResult<Network>>>,
    }

    impl MockNetworkReader {
        fn new(response: ClientResult<Network>) -> Self {
            Self {
                response: Mutex::new(Some(response)),
            }
        }
    }

    impl BitcoinNetworkReader for MockNetworkReader {
        async fn network(&self) -> ClientResult<Network> {
            self.response
                .lock()
                .expect("network response lock")
                .take()
                .expect("network response")
        }
    }

    #[test]
    fn test_transient_tip_failures_are_recoverable() {
        assert!(FetchBitcoinTipError::Rpc(ClientError::Timeout).is_recoverable());
        assert!(FetchBitcoinTipError::Rpc(ClientError::Server(
            BITCOIND_RPC_WARMUP,
            "warming up".to_owned(),
        ))
        .is_recoverable());
    }

    #[test]
    fn test_invalid_tip_failures_are_fatal() {
        assert!(!FetchBitcoinTipError::Rpc(ClientError::MissingUserPassword).is_recoverable());
        assert!(!FetchBitcoinTipError::HeightOutOfRange {
            tip_height: u64::MAX,
        }
        .is_recoverable());
    }

    #[tokio::test]
    async fn test_network_validation_accepts_expected_network() {
        let reader = MockNetworkReader::new(Ok(Network::Regtest));

        ensure_bitcoin_network(&reader, Network::Regtest)
            .await
            .expect("matching network should pass validation");
    }

    #[tokio::test]
    async fn test_network_validation_rejects_mismatch() {
        let reader = MockNetworkReader::new(Ok(Network::Bitcoin));

        let error = ensure_bitcoin_network(&reader, Network::Regtest)
            .await
            .expect_err("network mismatch should fail");

        assert!(matches!(
            error,
            BitcoinNetworkCheckError::NetworkMismatch {
                expected: Network::Regtest,
                actual: Network::Bitcoin,
            }
        ));
    }
}
