//! External capabilities used by the EE DA verifier service.

use std::future::Future;

use alpen_da_l1_extraction::{
    fetch_l1_block_range, FetchBlockError, FetchPolicy, FetchRangeError, L1BlockData,
    RecoveredDaBlob,
};
use alpen_database::{RecoveredDaDbError, RecoveredDaDbOps};
use bitcoind_async_client::Client;
use futures::Stream;
use strata_identifiers::L1Height;

use crate::bitcoin::{fetch_bitcoin_tip_height, FetchBitcoinTipError};

/// Operations the verifier performs against external systems.
pub(crate) trait DaVerifierContext: Send + Sync + 'static {
    /// Fetches the current Bitcoin chain tip height.
    fn fetch_bitcoin_tip_height(
        &self,
    ) -> impl Future<Output = Result<L1Height, FetchBitcoinTipError>> + Send;

    /// Streams an inclusive L1 block range in ascending height order.
    fn fetch_l1_block_range(
        &self,
        start_height: L1Height,
        end_height: L1Height,
    ) -> Result<impl Stream<Item = Result<L1BlockData, FetchBlockError>> + Send + '_, FetchRangeError>;

    /// Persists recovered EE DA blobs atomically.
    fn put_recovered_da(
        &self,
        recovered_blobs: Vec<RecoveredDaBlob>,
    ) -> impl Future<Output = Result<(), RecoveredDaDbError>> + Send;
}

/// Production [`DaVerifierContext`], backed by Bitcoin RPC and the
/// recovered-DA store.
pub(crate) struct DaVerifierContextImpl {
    bitcoin_client: Client,
    l1_block_fetch_policy: FetchPolicy,
    recovered_da_db: RecoveredDaDbOps,
}

impl DaVerifierContextImpl {
    /// Creates a verifier context from its Bitcoin and recovered-DA resources.
    pub(crate) fn new(
        bitcoin_client: Client,
        l1_block_fetch_policy: FetchPolicy,
        recovered_da_db: RecoveredDaDbOps,
    ) -> Self {
        Self {
            bitcoin_client,
            l1_block_fetch_policy,
            recovered_da_db,
        }
    }
}

impl DaVerifierContext for DaVerifierContextImpl {
    async fn fetch_bitcoin_tip_height(&self) -> Result<L1Height, FetchBitcoinTipError> {
        fetch_bitcoin_tip_height(&self.bitcoin_client).await
    }

    fn fetch_l1_block_range(
        &self,
        start_height: L1Height,
        end_height: L1Height,
    ) -> Result<impl Stream<Item = Result<L1BlockData, FetchBlockError>> + Send + '_, FetchRangeError>
    {
        fetch_l1_block_range(
            &self.bitcoin_client,
            start_height,
            end_height,
            &self.l1_block_fetch_policy,
        )
    }

    async fn put_recovered_da(
        &self,
        recovered_blobs: Vec<RecoveredDaBlob>,
    ) -> Result<(), RecoveredDaDbError> {
        self.recovered_da_db.put_async(recovered_blobs).await
    }
}
