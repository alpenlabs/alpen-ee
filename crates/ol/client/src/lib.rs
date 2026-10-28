//! The EE's clients for the OL chain: a JSON-RPC client and a dummy for testing without an OL node.

mod dummy;
mod rpc;

use alpen_ol_api::{
    AccountBlockSummary, AccountEpochSummary, AccountState, OLBlockClient, OLBlockHeader,
    OLChainStatus, OLClientError, OLEpochClient, OLSubmitClient,
};
use async_trait::async_trait;
use strata_identifiers::{Epoch, EpochCommitment, OLTxId};
use strata_snark_acct_types::SnarkAccountUpdate;

pub use self::{
    dummy::DummyOLClient,
    rpc::{RpcOLClient, STARTUP_RPC_MAX_RETRIES},
};

/// Enum wrapper that can hold either a real RPC client or a dummy client.
///
/// This allows runtime selection between the two client types while maintaining
/// the required trait bounds for use with the EE components.
#[derive(Debug)]
pub enum OLClientKind {
    /// Real RPC client connecting to an OL node.
    Rpc(RpcOLClient),
    /// Dummy client for testing without an OL node.
    Dummy(DummyOLClient),
}

#[async_trait]
impl OLEpochClient for OLClientKind {
    async fn chain_status(&self) -> Result<OLChainStatus, OLClientError> {
        match self {
            Self::Rpc(client) => client.chain_status().await,
            Self::Dummy(client) => client.chain_status().await,
        }
    }

    async fn account_epoch_summary(
        &self,
        epoch: Epoch,
    ) -> Result<AccountEpochSummary, OLClientError> {
        match self {
            Self::Rpc(client) => client.account_epoch_summary(epoch).await,
            Self::Dummy(client) => client.account_epoch_summary(epoch).await,
        }
    }

    async fn account_genesis_epoch(&self) -> Result<EpochCommitment, OLClientError> {
        match self {
            Self::Rpc(client) => client.account_genesis_epoch().await,
            Self::Dummy(client) => client.account_genesis_epoch().await,
        }
    }
}

#[async_trait]
impl OLBlockClient for OLClientKind {
    async fn account_block_summaries(
        &self,
        min_slot: u64,
        max_slot: u64,
    ) -> Result<Vec<AccountBlockSummary>, OLClientError> {
        match self {
            Self::Rpc(client) => client.account_block_summaries(min_slot, max_slot).await,
            Self::Dummy(client) => client.account_block_summaries(min_slot, max_slot).await,
        }
    }

    async fn block_header(&self, slot: u64) -> Result<OLBlockHeader, OLClientError> {
        match self {
            Self::Rpc(client) => client.block_header(slot).await,
            Self::Dummy(client) => client.block_header(slot).await,
        }
    }

    async fn latest_account_state(&self) -> Result<AccountState, OLClientError> {
        match self {
            Self::Rpc(client) => client.latest_account_state().await,
            Self::Dummy(client) => client.latest_account_state().await,
        }
    }
}

#[async_trait]
impl OLSubmitClient for OLClientKind {
    async fn submit_update(&self, update: SnarkAccountUpdate) -> Result<OLTxId, OLClientError> {
        match self {
            Self::Rpc(client) => client.submit_update(update).await,
            Self::Dummy(client) => client.submit_update(update).await,
        }
    }
}
