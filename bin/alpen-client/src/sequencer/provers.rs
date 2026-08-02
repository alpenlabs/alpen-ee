//! EE chunk + acct paas prover setup: task/receipt stores, the two
//! `ProverBuilder`s, and the backend (native vs. SP1) launch.
//!
//! Both provers use SP1 remote proving in production; native is dev-only
//! via the proofimpl crates' `native_host()`, used by functional tests.
//!
//! Storage layout (sled-backed, own sled db under `<datadir>/sled` — fully
//! separate from OL's; the prover trees live alongside the EE node trees):
//! - `task_store` — shared across both provers; task keys carry a kind tag (`b'c'`/`b'a'`) so chunk
//!   and batch entries don't collide in one tree.
//! - `chunk_receipts` — chunk prover writes (via paas auto-store); acct `fetch_input` reads back.
//! - `batch_proofs` — outer-proof store keyed by `BatchId`; outer hook writes, OL submission reads.
//!
//! All backed by `EeProverDbSled`; see `alpen_ee_database::sleddb::prover_db`
//! for schemas.

use std::sync::Arc;

use alpen_ee_common::{BatchStorage, ChunkStorage, SequencerOLClient};
use alpen_ee_database::{EeNodeStorage, SequencerDatabases};
use alpen_ee_params::AlpenParams;
use alpen_reth_witness::RangeWitnessExtractor;
use bitcoind_async_client::Client as BtcClient;
use reth_provider::{BlockReader, StateProviderFactory};
use strata_paas::{ProverBuilder, ReceiptStore, RetryConfig, TaskStore};
use tracing::info;

use super::prover::{
    launch_validated_ee_batch_prover, AcctRangeWitnessFn, AcctReceiptHook, AcctSpec,
    ChunkReceiptHook, ChunkSpec, EeBatchProofDbManager, EeChunkReceiptStore, EeProverBuilders,
    EeProverStores, EeProverTaskDbManager, PaasBatchProver,
};
use crate::{config::ProverBackendConfig, service_executor::ServiceExecutor};

/// Everything [`launch`] needs to build and launch the EE chunk + acct
/// provers.
pub(crate) struct EeProverInputs<P> {
    pub(crate) storage: Arc<EeNodeStorage>,
    pub(crate) node_provider: P,
    pub(crate) btc_client: Arc<BtcClient>,
    pub(crate) backend: ProverBackendConfig,
    pub(crate) params: Arc<AlpenParams>,
}

/// Builds the chunk + acct `ProverBuilder`s, picks a backend, validates the
/// resulting account predicate key against the OL's expected `update_vk`,
/// and launches both prover services.
pub(crate) async fn launch<P>(
    service_executor: &ServiceExecutor,
    dbs: &SequencerDatabases,
    ol_client: &(impl SequencerOLClient + Send + Sync),
    inputs: EeProverInputs<P>,
) -> eyre::Result<Arc<PaasBatchProver>>
where
    P: StateProviderFactory + BlockReader<Block = reth_primitives::Block> + Send + Sync + 'static,
{
    let EeProverInputs {
        storage,
        node_provider,
        btc_client,
        backend,
        params,
    } = inputs;

    let prover_db = dbs.prover_db();
    let task_store: Arc<dyn TaskStore> = Arc::new(EeProverTaskDbManager::new(prover_db.clone()));
    let chunk_receipts: Arc<dyn ReceiptStore> =
        Arc::new(EeChunkReceiptStore::new(prover_db.clone()));
    let batch_proofs = Arc::new(EeBatchProofDbManager::new(prover_db));
    let batch_storage_dyn: Arc<dyn BatchStorage> = storage.clone();
    let chunk_storage_dyn: Arc<dyn ChunkStorage> = storage.clone();

    let chunk_builder =
        ProverBuilder::new(ChunkSpec::new(chunk_storage_dyn.clone(), storage.clone()))
            .task_store(task_store.clone())
            .receipt_store(chunk_receipts.clone())
            .receipt_hook(ChunkReceiptHook::new(chunk_storage_dyn.clone()))
            .retry(RetryConfig::default());

    // TODO(STR-4157): the account prover still assembles its batch-range
    // witness via `RangeWitnessExtractor`, which builds a deep range
    // multiproof from per-block accessed-state records. Migrating to
    // inline per-block witnesses would let us drop this extractor and
    // the multiproof.
    let range_witness_extractor =
        Arc::new(RangeWitnessExtractor::new(node_provider, storage.clone()));
    let acct_range_witness_fn: Arc<AcctRangeWitnessFn> = {
        let extractor = range_witness_extractor.clone();
        Arc::new(move |first_block, last_block| {
            extractor.extract_range_witness(first_block, last_block)
        })
    };

    let acct_builder = ProverBuilder::new(AcctSpec::new(
        chunk_receipts.clone(),
        batch_storage_dyn.clone(),
        chunk_storage_dyn.clone(),
        storage.clone(),
        btc_client,
        dbs.witness_db(),
        acct_range_witness_fn,
    ))
    .task_store(task_store)
    .receipt_hook(AcctReceiptHook::new(
        batch_storage_dyn.clone(),
        batch_proofs.clone(),
    ))
    .retry(RetryConfig::default());

    let batch_prover = launch_validated_ee_batch_prover(
        ol_client,
        service_executor,
        EeProverBuilders {
            chunk: chunk_builder,
            account: acct_builder,
        },
        EeProverStores {
            chunk_storage: chunk_storage_dyn,
            batch_proofs,
        },
        backend,
        params,
    )
    .await?;

    info!(
        target: "alpen-client",
        component = "alpen",
        "EE chunk + acct paas provers started"
    );

    Ok(batch_prover)
}
