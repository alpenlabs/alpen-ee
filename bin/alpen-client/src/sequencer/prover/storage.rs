//! MDBX-backed storage managers for the EE prover.
//!
//! Three managers, all wrapping the shared [`EeProverDbMdbx`]:
//!
//! - [`EeTaskStore`] — impls `paas::TaskStore` over one task table for one resident spec version.
//!   The chunk prover gets an [`EeChunkTaskStore`], the acct prover an [`EeAcctTaskStore`].
//! - [`EeChunkReceiptStore`] — impls `paas::ReceiptStore`. The chunk prover writes here; the acct
//!   `fetch_input` reads from here.
//! - [`EeBatchProofDbManager`] — typed API keyed by [`BatchId`]; the outer (acct) prover writes
//!   here via its `ReceiptHook`, and the `BatchProver::get_proof(proof_id)` lookup is served from
//!   here.
//!
//! Parallels the OL pattern (`strata_storage::managers::{ProverTaskDbManager,
//! CheckpointProofDbManager}`) but lives in its own MDBX instance
//! under the alpen-client datadir — no cross-wiring with OL's
//! checkpoint storage.
//!
//! All methods are synchronous. MDBX ops are fast; PAAS drives these
//! from a background tick loop and its `ReceiptHook` is already async,
//! so calls from async contexts don't block meaningfully. No threadpool
//! layer for now — add one if this shows up in profiling.

use std::{marker::PhantomData, sync::Arc};

use alpen_ee_common::{decode_chunk_task_key, BatchId, Proof, ProofId};
use alpen_ee_database::{BatchTaskKey, ChunkTaskKey, EeProverDbMdbx, ProverTaskKey};
use alpen_ee_params::AlpenSpecId;
use strata_db_types::errors::DbError;
use strata_paas::{
    ProverError, ProverResult, ReceiptStore, TaskRecord, TaskRecordData, TaskStatus, TaskStore,
};
use zkaleido::ProofReceiptWithMetadata;

fn db_err(e: DbError) -> ProverError {
    match e {
        DbError::EntryAlreadyExists => ProverError::TaskAlreadyExists(String::new()),
        other => ProverError::Storage(other.to_string()),
    }
}

/// Task store of one prover kind for one resident spec version.
///
/// paas hands over the bare task bytes its `Task::into()` produced; this
/// store adds the version to select the row, so `tick`/`recover` in
/// `strata-paas`, which re-spawn work by listing the store, only ever see
/// the tasks this version's prover submitted. Without that scoping, one
/// resident version's poll loop could claim and sign a task meant for
/// another, proving it with the wrong VK. `K` selects the table, so a chunk
/// store never lists acct tasks and vice versa.
#[derive(Debug)]
pub(crate) struct EeTaskStore<K> {
    db: Arc<EeProverDbMdbx>,
    spec_version: AlpenSpecId,
    _key: PhantomData<K>,
}

/// The chunk prover's task store.
pub(crate) type EeChunkTaskStore = EeTaskStore<ChunkTaskKey>;

/// The acct prover's task store.
pub(crate) type EeAcctTaskStore = EeTaskStore<BatchTaskKey>;

impl<K> Clone for EeTaskStore<K> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            spec_version: self.spec_version,
            _key: PhantomData,
        }
    }
}

impl<K: ProverTaskKey> EeTaskStore<K> {
    pub(crate) fn new(db: Arc<EeProverDbMdbx>, spec_version: AlpenSpecId) -> Self {
        Self {
            db,
            spec_version,
            _key: PhantomData,
        }
    }

    fn key(&self, bytes: &[u8]) -> ProverResult<K> {
        K::from_task_bytes(self.spec_version, bytes)
            .map_err(|e| ProverError::Storage(format!("task key {bytes:?}: {e}")))
    }

    fn record(key: K, data: TaskRecordData) -> TaskRecord {
        TaskRecord::from_parts(key.task_bytes(), data)
    }

    fn modify<F>(&self, bytes: &[u8], f: F) -> ProverResult<()>
    where
        F: FnOnce(&mut TaskRecordData),
    {
        let key = self.key(bytes)?;
        let mut data = self
            .db
            .get_task(&key)
            .map_err(db_err)?
            .ok_or_else(|| ProverError::TaskNotFound(format!("{bytes:?}")))?;
        f(&mut data);
        self.db.put_task(&key, data).map_err(db_err)
    }
}

impl<K: ProverTaskKey> TaskStore for EeTaskStore<K> {
    fn get(&self, key: &[u8]) -> ProverResult<Option<TaskRecord>> {
        let stored = self.db.get_task(&self.key(key)?).map_err(db_err)?;
        Ok(stored.map(|data| TaskRecord::from_parts(key.to_vec(), data)))
    }

    fn insert(&self, record: TaskRecord) -> ProverResult<()> {
        let key = self.key(record.key())?;
        self.db
            .insert_task(&key, record.data().clone())
            .map_err(|e| match e {
                DbError::EntryAlreadyExists => {
                    ProverError::TaskAlreadyExists(format!("{:?}", record.key()))
                }
                other => ProverError::Storage(other.to_string()),
            })
    }

    fn update_status(&self, key: &[u8], status: TaskStatus) -> ProverResult<()> {
        self.modify(key, |d| d.set_status(status))
    }

    fn set_retry_after(&self, key: &[u8], when_secs: u64) -> ProverResult<()> {
        self.modify(key, |d| d.set_retry_after_secs(Some(when_secs)))
    }

    fn set_metadata(&self, key: &[u8], data: Vec<u8>) -> ProverResult<()> {
        self.modify(key, |d| d.set_metadata(Some(data)))
    }

    fn clear_metadata(&self, key: &[u8]) -> ProverResult<()> {
        self.modify(key, |d| d.set_metadata(None))
    }

    fn list_retriable(&self, now_secs: u64) -> ProverResult<Vec<TaskRecord>> {
        let items = self
            .db
            .list_retriable_tasks::<K>(self.spec_version, now_secs)
            .map_err(db_err)?;
        Ok(items.into_iter().map(|(k, d)| Self::record(k, d)).collect())
    }

    fn list_unfinished(&self) -> ProverResult<Vec<TaskRecord>> {
        let items = self
            .db
            .list_unfinished_tasks::<K>(self.spec_version)
            .map_err(db_err)?;
        Ok(items.into_iter().map(|(k, d)| Self::record(k, d)).collect())
    }

    fn count(&self) -> ProverResult<usize> {
        self.db.count_tasks::<K>(self.spec_version).map_err(db_err)
    }
}

/// MDBX-backed chunk receipt store.
///
/// Keyed by chunk task bytes (matches paas's `ReceiptStore`). The chunk
/// prover writes via its auto-store after proving; `AcctSpec::fetch_input`
/// reads via `collect_chunk_inputs_for_batch`.
#[derive(Debug, Clone)]
pub(crate) struct EeChunkReceiptStore {
    db: Arc<EeProverDbMdbx>,
}

impl EeChunkReceiptStore {
    pub(crate) fn new(db: Arc<EeProverDbMdbx>) -> Self {
        Self { db }
    }
}

impl ReceiptStore for EeChunkReceiptStore {
    fn put(&self, key: &[u8], receipt: &ProofReceiptWithMetadata) -> ProverResult<()> {
        let chunk_id = decode_chunk_task_key(key)
            .map_err(|e| ProverError::Storage(format!("chunk receipt key {key:?}: {e}")))?;
        self.db
            .put_chunk_receipt(chunk_id, receipt.clone())
            .map_err(db_err)
    }

    fn get(&self, key: &[u8]) -> ProverResult<Option<ProofReceiptWithMetadata>> {
        let chunk_id = decode_chunk_task_key(key)
            .map_err(|e| ProverError::Storage(format!("chunk receipt key {key:?}: {e}")))?;
        self.db.get_chunk_receipt(chunk_id).map_err(db_err)
    }
}

/// Typed outer-proof storage keyed by [`BatchId`].
///
/// MDBX-backed replacement for the earlier in-memory `HashMap` version.
/// The `AcctReceiptHook` writes here; `PaasBatchProver::get_proof(proof_id)`
/// serves OL submission from the secondary `ProofId → BatchId` index.
#[derive(Debug, Clone)]
pub(crate) struct EeBatchProofDbManager {
    db: Arc<EeProverDbMdbx>,
}

impl EeBatchProofDbManager {
    pub(crate) fn new(db: Arc<EeProverDbMdbx>) -> Self {
        Self { db }
    }

    /// `ProofId` for a batch — its `last_block` hash. Stable across
    /// in-memory and MDBX storage layers so the secondary index is
    /// a 1:1 map with the manager's public API.
    pub(crate) fn proof_id_for(batch_id: BatchId) -> ProofId {
        batch_id.last_block()
    }

    pub(crate) fn put_proof(
        &self,
        batch_id: BatchId,
        receipt: ProofReceiptWithMetadata,
    ) -> ProverResult<()> {
        self.db.put_acct_proof(batch_id, receipt).map_err(db_err)
    }

    pub(crate) fn has_proof(&self, batch_id: BatchId) -> bool {
        // MDBX errors surface as "not found"; callers treat this as a
        // storage-level concern and log separately.
        self.db.has_acct_proof(batch_id).unwrap_or(false)
    }

    pub(crate) fn get_proof_by_id(&self, proof_id: ProofId) -> Option<Proof> {
        let receipt = self.db.get_acct_proof_by_id(proof_id).ok().flatten()?;
        Some(Proof::from_vec(
            receipt.receipt().proof().as_bytes().to_vec(),
        ))
    }
}
