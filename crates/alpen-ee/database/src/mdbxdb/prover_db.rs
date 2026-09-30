//! MDBX-backed EE prover database: per-kind task tables and proof receipts.

use std::{path::Path, sync::Arc};

use alpen_ee_common::{BatchId, ChunkId, ProofId};
use alpen_ee_params::AlpenSpecId;
use alpen_store_mdbx::{MdbxConfig, MdbxEnv};
use strata_db_types::{errors::DbError, DbResult};
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use super::{
    schema::{
        prover_tables, AcctProofIdIndexSchema, AcctProofReceiptSchema, ChunkProofReceiptSchema,
    },
    task_key::ProverTaskKey,
    to_db_error,
};
use crate::serialization_types::{DBBatchId, DBChunkId};

fn proof_id_for(batch_id: BatchId) -> ProofId {
    batch_id.last_block()
}

/// EE prover storage: chunk and acct task tables, chunk receipts, and acct
/// proofs with their id index.
///
/// The task accessors are generic over [`ProverTaskKey`], which selects the
/// table: a [`ChunkTaskKey`](super::ChunkTaskKey) reads and writes the chunk
/// task table, a [`BatchTaskKey`](super::BatchTaskKey) the acct one. The
/// listing accessors take the spec version whose tasks to return, since every
/// resident prover program owns its own tasks.
#[derive(Debug)]
pub struct EeProverDbMdbx {
    env: Arc<MdbxEnv>,
}

impl EeProverDbMdbx {
    /// Wraps an already-open environment holding the prover tables.
    pub fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }

    /// Opens (or creates) the prover environment at `path`.
    pub fn open(path: &Path, config: &MdbxConfig) -> DbResult<Self> {
        let env = MdbxEnv::open(path, config, &prover_tables()).map_err(to_db_error)?;
        Ok(Self::new(Arc::new(env)))
    }

    // --- Tasks ---

    /// Reads the task stored under `key`.
    pub fn get_task<K: ProverTaskKey>(&self, key: &K) -> DbResult<Option<TaskRecordData>> {
        self.env.view(|r| K::get(r, key)).map_err(to_db_error)
    }

    /// Stores a new task, refusing to overwrite one already under `key`.
    ///
    /// Errs with [`DbError::EntryAlreadyExists`] when the key is taken.
    pub fn insert_task<K: ProverTaskKey>(&self, key: &K, record: TaskRecordData) -> DbResult<()> {
        let inserted = self
            .env
            .update(|w| {
                if K::get_in_write(w, key)?.is_some() {
                    return Ok(false);
                }
                K::put(w, key, &record)?;
                Ok(true)
            })
            .map_err(to_db_error)?;
        if inserted {
            Ok(())
        } else {
            Err(DbError::EntryAlreadyExists)
        }
    }

    /// Stores a task, replacing whatever was under `key`.
    pub fn put_task<K: ProverTaskKey>(&self, key: &K, record: TaskRecordData) -> DbResult<()> {
        self.env
            .update(|w| {
                K::put(w, key, &record)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    /// Removes the task under `key`, reporting whether one was there.
    pub fn delete_task<K: ProverTaskKey>(&self, key: &K) -> DbResult<bool> {
        self.env.update(|w| K::delete(w, key)).map_err(to_db_error)
    }

    /// Lists `spec_version`'s tasks that want a rescan and whose retry time
    /// has passed.
    pub fn list_retriable_tasks<K: ProverTaskKey>(
        &self,
        spec_version: AlpenSpecId,
        now_secs: u64,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_tasks_where::<K>(|key, record| {
            key.spec_version() == spec_version
                && record.status().wants_rescan()
                && record.retry_after_secs().is_some_and(|t| t <= now_secs)
        })
    }

    /// Lists `spec_version`'s tasks that have not reached a terminal status.
    pub fn list_unfinished_tasks<K: ProverTaskKey>(
        &self,
        spec_version: AlpenSpecId,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_tasks_where::<K>(|key, record| {
            key.spec_version() == spec_version && record.status().is_unfinished()
        })
    }

    /// Lists every task in `K`'s table, for every spec version.
    pub fn list_all_tasks<K: ProverTaskKey>(&self) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_tasks_where::<K>(|_, _| true)
    }

    /// Counts `spec_version`'s tasks in `K`'s table.
    pub fn count_tasks<K: ProverTaskKey>(&self, spec_version: AlpenSpecId) -> DbResult<usize> {
        self.env
            .view(|r| {
                let mut count = 0usize;
                K::for_each(r, |key, _| {
                    if key.spec_version() == spec_version {
                        count += 1;
                    }
                    Ok(())
                })?;
                Ok(count)
            })
            .map_err(to_db_error)
    }

    fn list_tasks_where<K: ProverTaskKey>(
        &self,
        mut keep: impl FnMut(&K, &TaskRecordData) -> bool,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.env
            .view(|r| {
                let mut out = Vec::new();
                K::for_each(r, |key, record| {
                    if keep(&key, &record) {
                        out.push((key, record));
                    }
                    Ok(())
                })?;
                Ok(out)
            })
            .map_err(to_db_error)
    }

    // --- Chunk receipts ---

    /// Stores the chunk proof receipt for `chunk_id`.
    pub fn put_chunk_receipt(
        &self,
        chunk_id: ChunkId,
        receipt: ProofReceiptWithMetadata,
    ) -> DbResult<()> {
        let db_id: DBChunkId = chunk_id.into();
        self.env
            .update(|w| {
                w.put::<ChunkProofReceiptSchema>(&db_id, &receipt)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    /// Reads the chunk proof receipt for `chunk_id`.
    pub fn get_chunk_receipt(
        &self,
        chunk_id: ChunkId,
    ) -> DbResult<Option<ProofReceiptWithMetadata>> {
        let db_id: DBChunkId = chunk_id.into();
        self.env
            .view(|r| r.get::<ChunkProofReceiptSchema>(&db_id))
            .map_err(to_db_error)
    }

    /// Removes the chunk proof receipt for `chunk_id`, reporting whether one
    /// was there.
    pub fn delete_chunk_receipt(&self, chunk_id: ChunkId) -> DbResult<bool> {
        let db_id: DBChunkId = chunk_id.into();
        self.env
            .update(|w| w.delete::<ChunkProofReceiptSchema>(&db_id))
            .map_err(to_db_error)
    }

    // --- Acct proofs ---

    /// Stores the acct proof receipt for `batch_id` and indexes it by proof id.
    pub fn put_acct_proof(
        &self,
        batch_id: BatchId,
        receipt: ProofReceiptWithMetadata,
    ) -> DbResult<()> {
        let db_id: DBBatchId = batch_id.into();
        let proof_id = proof_id_for(batch_id);
        let index_value: DBBatchId = batch_id.into();
        self.env
            .update(|w| {
                w.put::<AcctProofReceiptSchema>(&db_id, &receipt)?;
                w.put::<AcctProofIdIndexSchema>(&proof_id, &index_value)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    /// Reads the acct proof receipt for `batch_id`.
    pub fn get_acct_proof(&self, batch_id: BatchId) -> DbResult<Option<ProofReceiptWithMetadata>> {
        let db_id: DBBatchId = batch_id.into();
        self.env
            .view(|r| r.get::<AcctProofReceiptSchema>(&db_id))
            .map_err(to_db_error)
    }

    /// Whether an acct proof receipt is stored for `batch_id`.
    pub fn has_acct_proof(&self, batch_id: BatchId) -> DbResult<bool> {
        Ok(self.get_acct_proof(batch_id)?.is_some())
    }

    /// Reads the acct proof receipt indexed under `proof_id`.
    pub fn get_acct_proof_by_id(
        &self,
        proof_id: ProofId,
    ) -> DbResult<Option<ProofReceiptWithMetadata>> {
        self.env
            .view(|r| {
                let Some(db_id) = r.get::<AcctProofIdIndexSchema>(&proof_id)? else {
                    return Ok(None);
                };
                r.get::<AcctProofReceiptSchema>(&db_id)
            })
            .map_err(to_db_error)
    }

    /// Removes the acct proof receipt for `batch_id` and its index entry,
    /// reporting whether a receipt was there.
    pub fn delete_acct_proof(&self, batch_id: BatchId) -> DbResult<bool> {
        let db_id: DBBatchId = batch_id.into();
        let proof_id = proof_id_for(batch_id);
        self.env
            .update(|w| {
                let existed = w.delete::<AcctProofReceiptSchema>(&db_id)?;
                w.delete::<AcctProofIdIndexSchema>(&proof_id)?;
                Ok(existed)
            })
            .map_err(to_db_error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use strata_acct_types::Hash;
    use strata_paas::{AttemptCounts, TaskStatus};
    use zkaleido::{ProgramId, Proof, ProofMetadata, ProofReceipt, ProofType, PublicValues, ZkVm};

    use super::*;
    use crate::mdbxdb::{BatchTaskKey, ChunkTaskKey};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn setup_db() -> EeProverDbMdbx {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("ee-mdbx-prover-test-{}-{n}", process::id()));
        EeProverDbMdbx::open(&path, &MdbxConfig::small()).unwrap()
    }

    fn dummy_receipt() -> ProofReceiptWithMetadata {
        let receipt = ProofReceipt::new(Proof::default(), PublicValues::default());
        let metadata = ProofMetadata::new(
            ZkVm::Native,
            ProgramId([0u8; 32]),
            "0.1".to_string(),
            ProofType::Groth16,
        );
        ProofReceiptWithMetadata::new(receipt, metadata)
    }

    fn hash_from_u8(seed: u8) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[31] = seed;
        Hash::from(bytes)
    }

    fn chunk_key(spec_version: AlpenSpecId, seed: u8) -> ChunkTaskKey {
        ChunkTaskKey::new(
            spec_version,
            ChunkId::from_parts(hash_from_u8(seed), hash_from_u8(seed + 1)),
        )
    }

    fn batch_key(spec_version: AlpenSpecId, seed: u8) -> BatchTaskKey {
        BatchTaskKey::new(
            spec_version,
            BatchId::from_parts(hash_from_u8(seed), hash_from_u8(seed + 1)),
        )
    }

    #[test]
    fn delete_chunk_receipt_roundtrip() {
        let db = setup_db();
        let chunk_id = ChunkId::from_parts(hash_from_u8(1), hash_from_u8(2));

        assert!(matches!(db.delete_chunk_receipt(chunk_id), Ok(false)));

        db.put_chunk_receipt(chunk_id, dummy_receipt()).unwrap();
        assert!(db.get_chunk_receipt(chunk_id).unwrap().is_some());

        assert!(matches!(db.delete_chunk_receipt(chunk_id), Ok(true)));
        assert!(matches!(db.delete_chunk_receipt(chunk_id), Ok(false)));
        assert!(db.get_chunk_receipt(chunk_id).unwrap().is_none());
    }

    #[test]
    fn delete_acct_proof_clears_primary_and_secondary_rows() {
        let db = setup_db();
        let batch_id = BatchId::from_parts(hash_from_u8(1), hash_from_u8(2));
        let proof_id: ProofId = batch_id.last_block();

        assert!(matches!(db.delete_acct_proof(batch_id), Ok(false)));

        db.put_acct_proof(batch_id, dummy_receipt()).unwrap();
        assert!(db.has_acct_proof(batch_id).unwrap());
        assert!(db.get_acct_proof_by_id(proof_id).unwrap().is_some());

        assert!(matches!(db.delete_acct_proof(batch_id), Ok(true)));
        assert!(!db.has_acct_proof(batch_id).unwrap());
        assert!(db.get_acct_proof_by_id(proof_id).unwrap().is_none());
        assert!(matches!(db.delete_acct_proof(batch_id), Ok(false)));
    }

    #[test]
    fn insert_task_is_idempotent_guarded() {
        let db = setup_db();
        let key = chunk_key(AlpenSpecId::V0, 1);
        let record = TaskRecordData::new(TaskStatus::Pending);

        db.insert_task(&key, record.clone()).unwrap();
        assert!(db.get_task(&key).unwrap().is_some());
        assert!(matches!(
            db.insert_task(&key, record.clone()),
            Err(DbError::EntryAlreadyExists)
        ));

        db.put_task(&key, record).unwrap();
        assert_eq!(db.count_tasks::<ChunkTaskKey>(AlpenSpecId::V0).unwrap(), 1);

        assert!(db.delete_task(&key).unwrap());
        assert!(!db.delete_task(&key).unwrap());
        assert_eq!(db.count_tasks::<ChunkTaskKey>(AlpenSpecId::V0).unwrap(), 0);
    }

    #[test]
    fn chunk_and_batch_tasks_over_the_same_range_do_not_collide() {
        let db = setup_db();
        let chunk = chunk_key(AlpenSpecId::V0, 1);
        let batch = batch_key(AlpenSpecId::V0, 1);
        assert_eq!(chunk.task_bytes(), batch.task_bytes());

        db.insert_task(&chunk, TaskRecordData::new(TaskStatus::Pending))
            .unwrap();
        assert!(db.get_task(&batch).unwrap().is_none());
        assert!(db.list_all_tasks::<BatchTaskKey>().unwrap().is_empty());

        db.insert_task(&batch, TaskRecordData::new(TaskStatus::Completed))
            .unwrap();
        assert_eq!(
            db.get_task(&chunk).unwrap().unwrap().status(),
            &TaskStatus::Pending
        );
        assert_eq!(
            db.get_task(&batch).unwrap().unwrap().status(),
            &TaskStatus::Completed
        );
    }

    #[test]
    fn listings_are_scoped_to_the_spec_version() {
        let db = setup_db();
        let v0 = chunk_key(AlpenSpecId::V0, 1);
        let v1 = chunk_key(AlpenSpecId::V1, 1);
        assert_eq!(v0.chunk_id(), v1.chunk_id());

        db.insert_task(&v0, TaskRecordData::new(TaskStatus::Pending))
            .unwrap();
        let mut retriable = TaskRecordData::new(TaskStatus::TransientFailure {
            counts: AttemptCounts::default(),
            error: "boom".to_string(),
        });
        retriable.set_retry_after_secs(Some(10));
        db.insert_task(&v1, retriable).unwrap();

        // Pending is unfinished; a transient failure is retriable instead.
        let unfinished = db
            .list_unfinished_tasks::<ChunkTaskKey>(AlpenSpecId::V0)
            .unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].0, v0);
        assert!(db
            .list_unfinished_tasks::<ChunkTaskKey>(AlpenSpecId::V1)
            .unwrap()
            .is_empty());

        assert!(db
            .list_retriable_tasks::<ChunkTaskKey>(AlpenSpecId::V0, 100)
            .unwrap()
            .is_empty());
        let retriable = db
            .list_retriable_tasks::<ChunkTaskKey>(AlpenSpecId::V1, 100)
            .unwrap();
        assert_eq!(retriable.len(), 1);
        assert_eq!(retriable[0].0, v1);
        assert!(db
            .list_retriable_tasks::<ChunkTaskKey>(AlpenSpecId::V1, 5)
            .unwrap()
            .is_empty());

        assert_eq!(db.count_tasks::<ChunkTaskKey>(AlpenSpecId::V0).unwrap(), 1);
        assert_eq!(db.count_tasks::<ChunkTaskKey>(AlpenSpecId::V1).unwrap(), 1);
        assert_eq!(db.list_all_tasks::<ChunkTaskKey>().unwrap().len(), 2);
    }
}
