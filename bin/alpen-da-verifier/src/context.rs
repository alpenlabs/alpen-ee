//! External capabilities used by the EE DA verifier service.

use std::future::Future;

use alpen_da_l1_extraction::{
    fetch_l1_block_range, FetchBlockError, FetchPolicy, FetchRangeError, L1BlockData,
    RecoveredDaBlob,
};
use alpen_database::{RecoveredDaDbError, RecoveredDaDbOps};
use async_trait::async_trait;
use bitcoind_async_client::Client;
use futures::Stream;
use strata_acct_types::AccountId;
use strata_identifiers::L1Height;
use strata_snark_acct_types::Seqno;

use crate::{
    account_state::{OLAccountUpdate, OLAccountUpdateError, OLAccountUpdateSource},
    bitcoin::{fetch_bitcoin_tip_height, FetchBitcoinTipError},
    ol_rpc::RpcOLAccountUpdateSource,
};

/// Operations the verifier performs against external systems.
pub(crate) trait DaVerifierContext: OLAccountUpdateSource + Send + Sync + 'static {
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

    /// Loads the contiguous recovered prefix eligible at an L1 frontier.
    fn get_contiguous_recovered_da(
        &self,
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    ) -> impl Future<Output = Result<Vec<RecoveredDaBlob>, RecoveredDaDbError>> + Send;
}

/// Production [`DaVerifierContext`], backed by Bitcoin RPC, the
/// recovered-DA store, and OL RPC.
pub(crate) struct DaVerifierContextImpl {
    bitcoin_client: Client,
    l1_block_fetch_policy: FetchPolicy,
    recovered_da_db: RecoveredDaDbOps,
    account_update_source: RpcOLAccountUpdateSource,
}

impl DaVerifierContextImpl {
    /// Creates a verifier context from its external capabilities.
    pub(crate) fn new(
        bitcoin_client: Client,
        l1_block_fetch_policy: FetchPolicy,
        recovered_da_db: RecoveredDaDbOps,
        account_update_source: RpcOLAccountUpdateSource,
    ) -> Self {
        Self {
            bitcoin_client,
            l1_block_fetch_policy,
            recovered_da_db,
            account_update_source,
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

    async fn get_contiguous_recovered_da(
        &self,
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    ) -> Result<Vec<RecoveredDaBlob>, RecoveredDaDbError> {
        self.recovered_da_db
            .get_contiguous_from_async(first_update_seq_no, recovered_l1_frontier)
            .await
    }
}

#[async_trait]
impl OLAccountUpdateSource for DaVerifierContextImpl {
    async fn fetch_finalized_next_update_seq_no(
        &self,
        account_id: AccountId,
    ) -> Result<Seqno, OLAccountUpdateError> {
        self.account_update_source
            .fetch_finalized_next_update_seq_no(account_id)
            .await
    }

    async fn fetch_account_update(
        &self,
        account_id: AccountId,
        update_seq_no: Seqno,
    ) -> Result<OLAccountUpdate, OLAccountUpdateError> {
        self.account_update_source
            .fetch_account_update(account_id, update_seq_no)
            .await
    }
}
