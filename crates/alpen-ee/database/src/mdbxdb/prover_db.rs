//! MDBX-backed persistence for EE prover state.
//!
//! Provides a shared prover task store, chunk
//! proof receipts, and acct proof receipts with a `ProofId` secondary index.
//! Errors are mapped into [`strata_db_types::errors::DbError`] at the boundary;
//! the `EntryAlreadyExists` semantics of `insert_task` are expressed as an
//! in-transaction sentinel so the check-and-insert stays atomic under the
//! single MDBX writer.

use std::{path::Path, sync::Arc};

use alpen_db_store_mdbx::{MdbxConfig, MdbxEnv};
use alpen_ee_common::{BatchId, ProofId};
use strata_db_types::{errors::DbError, prover_task::ProverTaskDatabase, DbResult};
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use super::{
    schema::{
        prover_tables, AcctProofIdIndexSchema, AcctProofReceiptSchema, ChunkProofReceiptSchema,
        ProverTaskSchema,
    },
    to_db_error,
};
use crate::serialization_types::DBBatchId;

/// `ProofId` for a batch — its `last_block` hash.
fn proof_id_for(batch_id: BatchId) -> ProofId {
    batch_id.last_block()
}

/// Combined MDBX database for all prover-side persistence.
#[derive(Debug)]
pub struct EeProverDbMdbx {
    env: Arc<MdbxEnv>,
}

impl EeProverDbMdbx {
    /// Wraps an already-open environment whose tables include the prover tables.
    pub fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }

    /// Opens a standalone environment at `path` with just the prover tables.
    pub fn open(path: &Path, config: &MdbxConfig) -> DbResult<Self> {
        let env = MdbxEnv::open(path, config, &prover_tables()).map_err(to_db_error)?;
        Ok(Self::new(Arc::new(env)))
    }

    // ---- Chunk receipt store (paas::ReceiptStore shape) ----

    pub fn put_chunk_receipt(
        &self,
        key: Vec<u8>,
        receipt: ProofReceiptWithMetadata,
    ) -> DbResult<()> {
        self.env
            .update(|w| {
                w.put::<ChunkProofReceiptSchema>(&key, &receipt)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    pub fn get_chunk_receipt(&self, key: &[u8]) -> DbResult<Option<ProofReceiptWithMetadata>> {
        self.env
            .view(|r| r.get::<ChunkProofReceiptSchema>(&key.to_vec()))
            .map_err(to_db_error)
    }

    /// Removes a chunk receipt, returning `true` if a row existed.
    pub fn delete_chunk_receipt(&self, key: &[u8]) -> DbResult<bool> {
        self.env
            .update(|w| w.delete::<ChunkProofReceiptSchema>(&key.to_vec()))
            .map_err(to_db_error)
    }

    // ---- Acct proof store (typed BatchId API) ----

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

    pub fn get_acct_proof(&self, batch_id: BatchId) -> DbResult<Option<ProofReceiptWithMetadata>> {
        let db_id: DBBatchId = batch_id.into();
        self.env
            .view(|r| r.get::<AcctProofReceiptSchema>(&db_id))
            .map_err(to_db_error)
    }

    pub fn has_acct_proof(&self, batch_id: BatchId) -> DbResult<bool> {
        Ok(self.get_acct_proof(batch_id)?.is_some())
    }

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

    /// Removes an acct proof along with its secondary index entry, returning
    /// `true` if the proof row existed. Both deletes commit atomically.
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

impl ProverTaskDatabase for EeProverDbMdbx {
    fn get_task(&self, key: Vec<u8>) -> DbResult<Option<TaskRecordData>> {
        self.env
            .view(|r| r.get::<ProverTaskSchema>(&key))
            .map_err(to_db_error)
    }

    fn insert_task(&self, key: Vec<u8>, record: TaskRecordData) -> DbResult<()> {
        // Check-and-insert in one atomic write txn; signal an existing key via a
        // sentinel so the domain error stays out of the toolkit closure.
        let inserted = self
            .env
            .update(|w| {
                if w.get::<ProverTaskSchema>(&key)?.is_some() {
                    return Ok(false);
                }
                w.put::<ProverTaskSchema>(&key, &record)?;
                Ok(true)
            })
            .map_err(to_db_error)?;
        if inserted {
            Ok(())
        } else {
            Err(DbError::EntryAlreadyExists)
        }
    }

    fn put_task(&self, key: Vec<u8>, record: TaskRecordData) -> DbResult<()> {
        self.env
            .update(|w| {
                w.put::<ProverTaskSchema>(&key, &record)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    fn delete_task(&self, key: Vec<u8>) -> DbResult<bool> {
        self.env
            .update(|w| w.delete::<ProverTaskSchema>(&key))
            .map_err(to_db_error)
    }

    fn list_retriable(&self, now_secs: u64) -> DbResult<Vec<(Vec<u8>, TaskRecordData)>> {
        self.env
            .view(|r| {
                let mut out = Vec::new();
                r.for_each::<ProverTaskSchema>(|key, record| {
                    if record.status().is_retriable()
                        && record.retry_after_secs().is_some_and(|t| t <= now_secs)
                    {
                        out.push((key, record));
                    }
                    Ok(())
                })?;
                Ok(out)
            })
            .map_err(to_db_error)
    }

    fn list_unfinished(&self) -> DbResult<Vec<(Vec<u8>, TaskRecordData)>> {
        self.env
            .view(|r| {
                let mut out = Vec::new();
                r.for_each::<ProverTaskSchema>(|key, record| {
                    if record.status().is_unfinished() {
                        out.push((key, record));
                    }
                    Ok(())
                })?;
                Ok(out)
            })
            .map_err(to_db_error)
    }

    fn list_all_tasks(&self) -> DbResult<Vec<(Vec<u8>, TaskRecordData)>> {
        self.env
            .view(|r| {
                let mut out = Vec::new();
                r.for_each::<ProverTaskSchema>(|key, record| {
                    out.push((key, record));
                    Ok(())
                })?;
                Ok(out)
            })
            .map_err(to_db_error)
    }

    fn count_tasks(&self) -> DbResult<usize> {
        self.env
            .view(|r| {
                let mut count = 0usize;
                r.for_each::<ProverTaskSchema>(|_, _| {
                    count += 1;
                    Ok(())
                })?;
                Ok(count)
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
    use strata_paas::TaskStatus;
    use zkaleido::{ProgramId, Proof, ProofMetadata, ProofReceipt, ProofType, PublicValues, ZkVm};

    use super::*;

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

    #[test]
    fn delete_chunk_receipt_roundtrip() {
        let db = setup_db();
        let key = b"chunk-key".to_vec();

        assert!(matches!(db.delete_chunk_receipt(&key), Ok(false)));

        db.put_chunk_receipt(key.clone(), dummy_receipt()).unwrap();
        assert!(db.get_chunk_receipt(&key).unwrap().is_some());

        assert!(matches!(db.delete_chunk_receipt(&key), Ok(true)));
        assert!(matches!(db.delete_chunk_receipt(&key), Ok(false)));
        assert!(db.get_chunk_receipt(&key).unwrap().is_none());
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
        let key = b"task-1".to_vec();
        let record = TaskRecordData::new(TaskStatus::Pending);

        db.insert_task(key.clone(), record.clone()).unwrap();
        assert!(db.get_task(key.clone()).unwrap().is_some());

        // A second insert of the same key is rejected.
        assert!(matches!(
            db.insert_task(key.clone(), record.clone()),
            Err(DbError::EntryAlreadyExists)
        ));

        // put_task overwrites unconditionally.
        db.put_task(key.clone(), record).unwrap();
        assert_eq!(db.count_tasks().unwrap(), 1);

        assert!(db.delete_task(key.clone()).unwrap());
        assert!(!db.delete_task(key).unwrap());
        assert_eq!(db.count_tasks().unwrap(), 0);
    }
}
