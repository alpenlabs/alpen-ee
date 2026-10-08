//! External capabilities used by the EE DA verifier service.

use std::{future::Future, path::PathBuf};

use alpen_batch_replay::BatchReplaySnapshot;
use alpen_da_l1_extraction::{
    fetch_l1_block_range, FetchBlockError, FetchPolicy, FetchRangeError, L1BlockData,
    RecoveredDaBlob,
};
use alpen_database::{RecoveredDaDbError, RecoveredDaDbOps};
use async_trait::async_trait;
use bitcoin::Txid;
use bitcoind_async_client::Client;
use futures::Stream;
use strata_acct_types::AccountId;
use strata_identifiers::{L1BlockCommitment, L1Height};
use strata_snark_acct_types::Seqno;
use tokio::task::block_in_place;

use crate::{
    account_state::{
        OLAccountUpdate, OLAccountUpdateError, OLAccountUpdateSource, VerifiedAccountState,
    },
    bitcoin::{
        fetch_bitcoin_tip_height, fetch_commit_block, is_l1_block_canonical, CheckL1BlockError,
        FetchBitcoinTipError, FetchCommitBlockError,
    },
    ol_rpc::RpcOLAccountUpdateSource,
    snapshot::{
        delete_reconstruction_snapshot, load_reconstruction_snapshot, save_reconstruction_snapshot,
        ReconstructionSnapshot, SnapshotDeleteError, SnapshotLoadError, SnapshotSaveError,
    },
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

    /// Removes recovered DA with sequence numbers below an exclusive boundary.
    fn prune_recovered_da_before(
        &self,
        update_seq_no: u64,
    ) -> impl Future<Output = Result<(), RecoveredDaDbError>> + Send;

    /// Removes all recovered DA.
    fn clear_recovered_da(&self) -> impl Future<Output = Result<(), RecoveredDaDbError>> + Send;

    /// Returns the L1 block containing an EE DA commit transaction.
    fn fetch_commit_block(
        &self,
        commit_txid: Txid,
    ) -> impl Future<Output = Result<L1BlockCommitment, FetchCommitBlockError>> + Send;

    /// Atomically persists one verified reconstruction snapshot.
    fn save_reconstruction_snapshot(
        &self,
        replay_snapshot: &BatchReplaySnapshot,
        verified_account_state: &VerifiedAccountState,
        resume_l1_block: L1BlockCommitment,
        completion_block: L1BlockCommitment,
    ) -> Result<(), SnapshotSaveError>;

    /// Returns whether an L1 block commitment is canonical at its recorded height.
    fn is_l1_block_canonical(
        &self,
        expected: L1BlockCommitment,
    ) -> impl Future<Output = Result<bool, CheckL1BlockError>> + Send;

    /// Loads the reconstruction snapshot, returning [`None`] when it does not exist.
    fn load_reconstruction_snapshot(
        &self,
    ) -> Result<Option<ReconstructionSnapshot>, SnapshotLoadError>;

    /// Deletes the reconstruction snapshot.
    fn delete_reconstruction_snapshot(&self) -> Result<(), SnapshotDeleteError>;
}

/// Production [`DaVerifierContext`], backed by Bitcoin RPC, the
/// recovered-DA store, and OL RPC.
pub(crate) struct DaVerifierContextImpl {
    bitcoin_client: Client,
    l1_block_fetch_policy: FetchPolicy,
    recovered_da_db: RecoveredDaDbOps,
    account_update_source: RpcOLAccountUpdateSource,
    snapshot_path: PathBuf,
}

impl DaVerifierContextImpl {
    /// Creates a verifier context from its external capabilities.
    pub(crate) fn new(
        bitcoin_client: Client,
        l1_block_fetch_policy: FetchPolicy,
        recovered_da_db: RecoveredDaDbOps,
        account_update_source: RpcOLAccountUpdateSource,
        snapshot_path: PathBuf,
    ) -> Self {
        Self {
            bitcoin_client,
            l1_block_fetch_policy,
            recovered_da_db,
            account_update_source,
            snapshot_path,
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

    async fn prune_recovered_da_before(
        &self,
        update_seq_no: u64,
    ) -> Result<(), RecoveredDaDbError> {
        self.recovered_da_db.prune_before_async(update_seq_no).await
    }

    async fn clear_recovered_da(&self) -> Result<(), RecoveredDaDbError> {
        self.recovered_da_db.clear_async().await
    }

    async fn fetch_commit_block(
        &self,
        commit_txid: Txid,
    ) -> Result<L1BlockCommitment, FetchCommitBlockError> {
        fetch_commit_block(&self.bitcoin_client, commit_txid).await
    }

    async fn is_l1_block_canonical(
        &self,
        expected: L1BlockCommitment,
    ) -> Result<bool, CheckL1BlockError> {
        is_l1_block_canonical(&self.bitcoin_client, expected).await
    }

    fn load_reconstruction_snapshot(
        &self,
    ) -> Result<Option<ReconstructionSnapshot>, SnapshotLoadError> {
        block_in_place(|| load_reconstruction_snapshot(&self.snapshot_path))
    }

    fn delete_reconstruction_snapshot(&self) -> Result<(), SnapshotDeleteError> {
        block_in_place(|| delete_reconstruction_snapshot(&self.snapshot_path))
    }

    fn save_reconstruction_snapshot(
        &self,
        replay_snapshot: &BatchReplaySnapshot,
        verified_account_state: &VerifiedAccountState,
        resume_l1_block: L1BlockCommitment,
        completion_block: L1BlockCommitment,
    ) -> Result<(), SnapshotSaveError> {
        block_in_place(|| {
            save_reconstruction_snapshot(
                &self.snapshot_path,
                replay_snapshot,
                verified_account_state,
                resume_l1_block,
                completion_block,
            )
        })
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
