//! Sled-backed storage managers for the EE prover.
//!
//! Three managers, all wrapping the shared [`EeProverDbSled`]:
//!
//! - [`EeProverTaskDbManager`] — impls `paas::TaskStore`. Shared across chunk + acct provers via
//!   the kind-tagged task-key encoding (see `CHUNK_TASK_KEY_TAG` / `BATCH_TASK_KEY_TAG`).
//! - [`EeChunkReceiptStore`] — impls `paas::ReceiptStore`. The chunk prover writes here; the acct
//!   `fetch_input` reads from here.
//! - [`EeBatchProofDbManager`] — typed API keyed by [`BatchId`]; the outer (acct) prover writes
//!   here via its `ReceiptHook`, and the `BatchProver::get_proof(proof_id)` lookup is served from
//!   here.
//!
//! Parallels the OL pattern (`strata_storage::managers::{ProverTaskDbManager,
//! CheckpointProofDbManager}`) but lives in its own sled instance
//! under the alpen-client datadir — no cross-wiring with OL's
//! checkpoint storage.
//!
//! All methods are synchronous. Sled ops are fast; PAAS drives these
//! from a background tick loop and its `ReceiptHook` is already async,
//! so calls from async contexts don't block meaningfully. No threadpool
//! layer for now — add one if this shows up in profiling.

use std::sync::Arc;

use alpen_ee_common::{BatchId, Proof, ProofId};
use alpen_ee_database::EeProverDbSled;
use alpen_ee_params::AlpenSpecId;
use strata_db_types::{errors::DbError, prover_task::ProverTaskDatabase};
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

/// Sled-backed shared prover task store.
///
/// Both chunk and acct provers hold an `Arc<Self>` and pass it to
/// `ProverBuilder::task_store(...)`. Task keys carry a single-byte
/// kind tag (`b'c'` / `b'a'`) inside their `Task::into()` encoding,
/// so entries from the two provers don't collide in the shared tree.
#[derive(Debug, Clone)]
pub(crate) struct EeProverTaskDbManager {
    db: Arc<EeProverDbSled>,
}

impl EeProverTaskDbManager {
    pub(crate) fn new(db: Arc<EeProverDbSled>) -> Self {
        Self { db }
    }

    fn modify<F>(&self, key: &[u8], f: F) -> ProverResult<()>
    where
        F: FnOnce(&mut TaskRecordData),
    {
        let mut data = self
            .db
            .get_task(key.to_vec())
            .map_err(db_err)?
            .ok_or_else(|| ProverError::TaskNotFound(format!("{:?}", key)))?;
        f(&mut data);
        self.db.put_task(key.to_vec(), data).map_err(db_err)
    }
}

impl TaskStore for EeProverTaskDbManager {
    fn get(&self, key: &[u8]) -> ProverResult<Option<TaskRecord>> {
        let stored = self.db.get_task(key.to_vec()).map_err(db_err)?;
        Ok(stored.map(|data| TaskRecord::from_parts(key.to_vec(), data)))
    }

    fn insert(&self, record: TaskRecord) -> ProverResult<()> {
        let (key, data) = (record.key().to_vec(), record.data().clone());
        self.db.insert_task(key.clone(), data).map_err(|e| match e {
            DbError::EntryAlreadyExists => ProverError::TaskAlreadyExists(format!("{:?}", key)),
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

    fn list_retriable(&self, now_secs: u64) -> ProverResult<Vec<TaskRecord>> {
        let items = self.db.list_retriable(now_secs).map_err(db_err)?;
        Ok(items
            .into_iter()
            .map(|(k, d)| TaskRecord::from_parts(k, d))
            .collect())
    }

    fn list_unfinished(&self) -> ProverResult<Vec<TaskRecord>> {
        let items = self.db.list_unfinished().map_err(db_err)?;
        Ok(items
            .into_iter()
            .map(|(k, d)| TaskRecord::from_parts(k, d))
            .collect())
    }

    fn count(&self) -> ProverResult<usize> {
        self.db.count_tasks().map_err(db_err)
    }
}

/// Scopes a shared [`TaskStore`] to the tasks submitted for one resident
/// spec version.
///
/// Chunk and acct tasks for every resident `--prover-program` candidate
/// share one physical sled tree ([`EeProverTaskDbManager`]'s doc comment).
/// That's fine for `get`/`insert`/`update_status`, which are always called
/// with a specific key. But `Prover::tick`/`recover` (in `strata-paas`)
/// re-spawn work by scanning the *entire* task store for retriable/
/// unfinished records, with no notion of which resident version's `Prover`
/// submitted a given task. If two versions' `Prover<H>` instances shared
/// that store directly, either one's background poll loop could claim and
/// sign a task meant for the other -- proving it with the wrong VK. This
/// wrapper prefixes every physical key with the version's discriminant
/// before touching the shared store, and strips the prefix back off before
/// handing records to paas, so `decode_task_key::<H>` still sees exactly
/// the bytes `H::Task::into()` produced: each version's `tick`/`recover`
/// only ever observes its own tasks.
#[derive(Clone)]
pub(crate) struct VersionedTaskStore {
    inner: Arc<dyn TaskStore>,
    prefix: [u8; 2],
}

impl VersionedTaskStore {
    pub(crate) fn new(inner: Arc<dyn TaskStore>, version: AlpenSpecId) -> Self {
        Self {
            inner,
            prefix: u16::from(version).to_be_bytes(),
        }
    }

    fn prefixed(&self, key: &[u8]) -> Vec<u8> {
        let mut prefixed = Vec::with_capacity(self.prefix.len() + key.len());
        prefixed.extend_from_slice(&self.prefix);
        prefixed.extend_from_slice(key);
        prefixed
    }

    /// Strips this instance's prefix off a record fetched from the shared
    /// store, or `None` if the record belongs to a different version.
    fn strip_prefix(&self, record: TaskRecord) -> Option<TaskRecord> {
        let stripped = record.key().strip_prefix(self.prefix.as_slice())?.to_vec();
        Some(TaskRecord::from_parts(stripped, record.data().clone()))
    }
}

impl TaskStore for VersionedTaskStore {
    fn get(&self, key: &[u8]) -> ProverResult<Option<TaskRecord>> {
        Ok(self
            .inner
            .get(&self.prefixed(key))?
            .map(|record| TaskRecord::from_parts(key.to_vec(), record.data().clone())))
    }

    fn insert(&self, record: TaskRecord) -> ProverResult<()> {
        let prefixed_key = self.prefixed(record.key());
        self.inner
            .insert(TaskRecord::from_parts(prefixed_key, record.data().clone()))
    }

    fn update_status(&self, key: &[u8], status: TaskStatus) -> ProverResult<()> {
        self.inner.update_status(&self.prefixed(key), status)
    }

    fn set_retry_after(&self, key: &[u8], when_secs: u64) -> ProverResult<()> {
        self.inner.set_retry_after(&self.prefixed(key), when_secs)
    }

    fn set_metadata(&self, key: &[u8], data: Vec<u8>) -> ProverResult<()> {
        self.inner.set_metadata(&self.prefixed(key), data)
    }

    fn list_retriable(&self, now_secs: u64) -> ProverResult<Vec<TaskRecord>> {
        Ok(self
            .inner
            .list_retriable(now_secs)?
            .into_iter()
            .filter_map(|record| self.strip_prefix(record))
            .collect())
    }

    fn list_unfinished(&self) -> ProverResult<Vec<TaskRecord>> {
        Ok(self
            .inner
            .list_unfinished()?
            .into_iter()
            .filter_map(|record| self.strip_prefix(record))
            .collect())
    }

    /// Approximate: counts only this version's *unfinished* tasks, since the
    /// shared store exposes no prefix-scoped total count. Diagnostic-only
    /// today (nothing in this codebase calls `TaskStore::count`).
    fn count(&self) -> ProverResult<usize> {
        Ok(self.list_unfinished()?.len())
    }
}

/// Sled-backed chunk receipt store.
///
/// Keyed by chunk task bytes (matches paas's `ReceiptStore`). The chunk
/// prover writes via its auto-store after proving; `AcctSpec::fetch_input`
/// reads via `collect_chunk_inputs_for_batch`.
#[derive(Debug, Clone)]
pub(crate) struct EeChunkReceiptStore {
    db: Arc<EeProverDbSled>,
}

impl EeChunkReceiptStore {
    pub(crate) fn new(db: Arc<EeProverDbSled>) -> Self {
        Self { db }
    }
}

impl ReceiptStore for EeChunkReceiptStore {
    fn put(&self, key: &[u8], receipt: &ProofReceiptWithMetadata) -> ProverResult<()> {
        self.db
            .put_chunk_receipt(key.to_vec(), receipt.clone())
            .map_err(db_err)
    }

    fn get(&self, key: &[u8]) -> ProverResult<Option<ProofReceiptWithMetadata>> {
        self.db.get_chunk_receipt(key).map_err(db_err)
    }
}

/// Typed outer-proof storage keyed by [`BatchId`].
///
/// Sled-backed replacement for the earlier in-memory `HashMap` version.
/// The `AcctReceiptHook` writes here; `PaasBatchProver::get_proof(proof_id)`
/// serves OL submission from the secondary `ProofId → BatchId` index.
#[derive(Debug, Clone)]
pub(crate) struct EeBatchProofDbManager {
    db: Arc<EeProverDbSled>,
}

impl EeBatchProofDbManager {
    pub(crate) fn new(db: Arc<EeProverDbSled>) -> Self {
        Self { db }
    }

    /// `ProofId` for a batch — its `last_block` hash. Stable across
    /// in-memory and sled storage layers so the secondary index is
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
        // sled errors surface as "not found"; callers treat this as a
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
