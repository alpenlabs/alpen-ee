//! Bitcoin RPC helpers for the EE DA verifier.

use std::future::Future;

use anyhow::Context;
use bitcoin::{Network, Txid};
use bitcoind_async_client::{
    client::Auth, error::ClientError, traits::Reader, Client, ClientResult,
};
use strata_btc_types::BlockHashExt;
use strata_identifiers::{L1BlockCommitment, L1Height};
use thiserror::Error;

use crate::{args::BitcoinRpcCredentials, config::BitcoindConfig};

/// Bitcoind RPC error code returned while the node is warming up.
const BITCOIND_RPC_WARMUP: i32 = -28;
/// Bitcoind RPC error code returned when `getblockhash` is above the current tip.
const BITCOIND_RPC_INVALID_PARAMETER: i32 = -8;
/// Bitcoind RPC error code returned when a transaction is unavailable.
const BITCOIND_RPC_INVALID_ADDRESS_OR_KEY: i32 = -5;

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
    /// Returns whether a later recovery attempt may succeed without intervention.
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

/// Failure to locate the L1 block containing an EE DA commit transaction.
#[derive(Debug, Error)]
pub(crate) enum FetchCommitBlockError {
    /// Fetching the commit transaction failed.
    #[error("failed to fetch commit transaction {txid}: {source}")]
    FetchTransaction {
        txid: Txid,
        #[source]
        source: ClientError,
    },

    /// Bitcoind cannot locate a commit transaction expected to remain canonical.
    #[error(
        "commit transaction {txid} is unavailable; ensure bitcoind has txindex enabled and the transaction remains canonical: {source}"
    )]
    TransactionUnavailable {
        txid: Txid,
        #[source]
        source: ClientError,
    },

    /// The commit transaction has not been confirmed in an L1 block.
    #[error("commit transaction {txid} is not confirmed")]
    Unconfirmed { txid: Txid },

    /// Fetching the commit transaction's block height failed.
    #[error("failed to fetch block height for commit transaction {txid}: {source}")]
    FetchBlockHeight {
        txid: Txid,
        #[source]
        source: ClientError,
    },

    /// The returned block height does not fit the supported L1 height type.
    #[error("block height {height} for commit transaction {txid} exceeds the L1 height range")]
    HeightOutOfRange { txid: Txid, height: u64 },
}

/// Failure to compare a saved L1 commitment with Bitcoin's canonical chain.
#[derive(Debug, Error)]
pub(crate) enum CheckL1BlockError {
    /// Fetching the canonical block hash failed.
    #[error("failed to fetch canonical L1 block at height {}: {source}", .expected.height())]
    FetchBlockHash {
        expected: L1BlockCommitment,
        #[source]
        source: ClientError,
    },
}

impl CheckL1BlockError {
    /// Returns whether a later canonicality check may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchBlockHash { source, .. } => {
                is_recoverable_rpc_error(source)
                    || matches!(
                        source,
                        ClientError::Server(BITCOIND_RPC_INVALID_PARAMETER, _)
                    )
            }
        }
    }
}

impl FetchCommitBlockError {
    /// Returns whether a later lookup may succeed without intervention.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::FetchTransaction { source, .. } | Self::FetchBlockHeight { source, .. } => {
                is_recoverable_rpc_error(source)
            }
            Self::Unconfirmed { .. } => true,
            Self::TransactionUnavailable { .. } | Self::HeightOutOfRange { .. } => false,
        }
    }
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

fn map_fetch_transaction_error(txid: Txid, source: ClientError) -> FetchCommitBlockError {
    match source {
        source @ ClientError::Server(BITCOIND_RPC_INVALID_ADDRESS_OR_KEY, _) => {
            // Bitcoind also uses this code for an unknown transaction. Snapshot provenance comes
            // from reorg-safe recovered DA, so persistent absence indicates an unusable endpoint
            // or broken canonical-chain assumption rather than a condition to retry indefinitely.
            FetchCommitBlockError::TransactionUnavailable { txid, source }
        }
        source => FetchCommitBlockError::FetchTransaction { txid, source },
    }
}

/// Returns the L1 block containing the supplied EE DA commit transaction.
pub(crate) async fn fetch_commit_block(
    client: &Client,
    txid: Txid,
) -> Result<L1BlockCommitment, FetchCommitBlockError> {
    let transaction = client
        .get_raw_transaction_verbosity_one(&txid)
        .await
        .map_err(|source| map_fetch_transaction_error(txid, source))?;
    let block_hash = transaction
        .block_hash
        .ok_or(FetchCommitBlockError::Unconfirmed { txid })?;
    let height = client
        .get_block_height(&block_hash)
        .await
        .map_err(|source| FetchCommitBlockError::FetchBlockHeight { txid, source })?;
    let height = L1Height::try_from(height)
        .map_err(|_| FetchCommitBlockError::HeightOutOfRange { txid, height })?;

    Ok(L1BlockCommitment::new(height, block_hash.to_l1_block_id()))
}

/// Returns whether `expected` is canonical at its recorded L1 height.
pub(crate) async fn is_l1_block_canonical(
    client: &Client,
    expected: L1BlockCommitment,
) -> Result<bool, CheckL1BlockError> {
    let actual_hash = client
        .get_block_hash(u64::from(expected.height()))
        .await
        .map_err(|source| CheckL1BlockError::FetchBlockHash { expected, source })?;
    Ok(actual_hash.to_l1_block_id() == *expected.blkid())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bitcoin::hashes::Hash as _;

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
    async fn test_network_validation_propagates_rpc_failure() {
        let reader = MockNetworkReader::new(Err(ClientError::Timeout));

        let error = ensure_bitcoin_network(&reader, Network::Regtest)
            .await
            .expect_err("network RPC failure should fail validation");

        assert!(matches!(
            error,
            BitcoinNetworkCheckError::Rpc(ClientError::Timeout)
        ));
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

    #[test]
    fn test_fetch_transaction_failure_follows_rpc_classification() {
        assert!(FetchCommitBlockError::FetchTransaction {
            txid: Txid::all_zeros(),
            source: ClientError::Timeout,
        }
        .is_recoverable());
        assert!(!FetchCommitBlockError::FetchTransaction {
            txid: Txid::all_zeros(),
            source: ClientError::MissingUserPassword,
        }
        .is_recoverable());
    }

    #[test]
    fn test_transaction_unavailable_is_fatal() {
        assert!(!FetchCommitBlockError::TransactionUnavailable {
            txid: Txid::all_zeros(),
            source: ClientError::Server(
                BITCOIND_RPC_INVALID_ADDRESS_OR_KEY,
                "No such mempool or blockchain transaction".to_owned(),
            ),
        }
        .is_recoverable());
    }

    #[test]
    fn test_unconfirmed_commit_is_recoverable() {
        assert!(FetchCommitBlockError::Unconfirmed {
            txid: Txid::all_zeros(),
        }
        .is_recoverable());
    }

    #[test]
    fn test_fetch_block_height_failure_follows_rpc_classification() {
        assert!(FetchCommitBlockError::FetchBlockHeight {
            txid: Txid::all_zeros(),
            source: ClientError::Timeout,
        }
        .is_recoverable());
        assert!(!FetchCommitBlockError::FetchBlockHeight {
            txid: Txid::all_zeros(),
            source: ClientError::MissingUserPassword,
        }
        .is_recoverable());
    }

    #[test]
    fn test_commit_block_height_out_of_range_is_fatal() {
        assert!(!FetchCommitBlockError::HeightOutOfRange {
            txid: Txid::all_zeros(),
            height: u64::MAX,
        }
        .is_recoverable());
    }

    #[test]
    fn test_invalid_address_error_maps_to_transaction_unavailable() {
        let txid = Txid::all_zeros();

        let error = map_fetch_transaction_error(
            txid,
            ClientError::Server(
                BITCOIND_RPC_INVALID_ADDRESS_OR_KEY,
                "No such mempool or blockchain transaction".to_owned(),
            ),
        );

        assert!(matches!(
            error,
            FetchCommitBlockError::TransactionUnavailable {
                txid: actual_txid,
                ..
            } if actual_txid == txid
        ));
    }

    #[test]
    fn test_other_transaction_error_maps_to_fetch_failure() {
        let txid = Txid::all_zeros();

        let error = map_fetch_transaction_error(txid, ClientError::Timeout);

        assert!(matches!(
            error,
            FetchCommitBlockError::FetchTransaction {
                txid: actual_txid,
                source: ClientError::Timeout,
            } if actual_txid == txid
        ));
    }

    #[test]
    fn test_canonicality_failures_follow_rpc_classification() {
        let expected = L1BlockCommitment::default();

        assert!(CheckL1BlockError::FetchBlockHash {
            expected,
            source: ClientError::Timeout,
        }
        .is_recoverable());
        assert!(CheckL1BlockError::FetchBlockHash {
            expected,
            source: ClientError::Server(
                BITCOIND_RPC_INVALID_PARAMETER,
                "Block height out of range".to_owned(),
            ),
        }
        .is_recoverable());
        assert!(!CheckL1BlockError::FetchBlockHash {
            expected,
            source: ClientError::MissingUserPassword,
        }
        .is_recoverable());
    }
}
