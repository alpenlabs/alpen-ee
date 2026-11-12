//! MDBX-backed EE prover database: per-kind task tables and proof receipts.

use std::{path::Path, sync::Arc};

use alpen_common::{BatchId, ChunkId, ProofId, ProverTaskKeyDecodeError};
use alpen_params::AlpenSpecId;
use alpen_storage_mdbx::{DbResult as MdbxResult, MdbxConfig, MdbxEnv, Schema};
use strata_db_types::{errors::DbError, DbResult};
use strata_paas::TaskRecordData;
use zkaleido::ProofReceiptWithMetadata;

use super::{
    schema::{
        prover_tables, AcctProofIdIndexSchema, AcctProofReceiptSchema, AcctProverTaskSchema,
        ChunkProofReceiptSchema, ChunkProverTaskSchema,
    },
    to_db_error,
};
use crate::serialization_types::{
    BatchTaskKey, ChunkTaskKey, DBBatchId, DBChunkId, DBProverTask, ProgramScope,
};

mod sealed {
    /// Closed: the set of task tables is the two below.
    pub trait Sealed {}
}

/// A key of one of the prover task tables, which names the table it lives in.
///
/// Implemented by [`ChunkTaskKey`] and [`BatchTaskKey`] only. The store's
/// task accessors are generic over this, so one implementation serves both
/// kinds while `K::Table` keeps a chunk key out of the acct table and vice
/// versa. The paas-bytes conversions are the key types' own; the trait only
/// exposes them to generic code. See the `task_key` module of the record
/// types for the layout and the reasoning.
pub trait ProverTaskKey: sealed::Sealed + Copy + Send + Sync + 'static {
    /// The table holding this kind's tasks.
    type Table: Schema<Key = Self, Value = DBProverTask>;

    /// The spec version whose prover owns the task.
    fn spec_version(&self) -> AlpenSpecId;

    /// Adds `spec_version` to a paas task key (the bare range bytes).
    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError>;

    /// The paas task key: the bare range bytes, without the version.
    fn task_bytes(&self) -> Vec<u8>;
}

impl sealed::Sealed for ChunkTaskKey {}

impl ProverTaskKey for ChunkTaskKey {
    type Table = ChunkProverTaskSchema;

    fn spec_version(&self) -> AlpenSpecId {
        ChunkTaskKey::spec_version(self)
    }

    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        ChunkTaskKey::from_task_bytes(spec_version, bytes)
    }

    fn task_bytes(&self) -> Vec<u8> {
        ChunkTaskKey::task_bytes(self)
    }
}

impl sealed::Sealed for BatchTaskKey {}

impl ProverTaskKey for BatchTaskKey {
    type Table = AcctProverTaskSchema;

    fn spec_version(&self) -> AlpenSpecId {
        BatchTaskKey::spec_version(self)
    }

    fn from_task_bytes(
        spec_version: AlpenSpecId,
        bytes: &[u8],
    ) -> Result<Self, ProverTaskKeyDecodeError> {
        BatchTaskKey::from_task_bytes(spec_version, bytes)
    }

    fn task_bytes(&self) -> Vec<u8> {
        BatchTaskKey::task_bytes(self)
    }
}

fn proof_id_for(batch_id: BatchId) -> ProofId {
    batch_id.last_block()
}

/// EE prover storage: chunk and acct task tables, chunk receipts, and acct
/// proofs with their id index.
///
/// The task accessors are generic over [`ProverTaskKey`], whose `Table`
/// selects the table: a [`ChunkTaskKey`] reads and writes the chunk task
/// table, a [`BatchTaskKey`] the acct one. The listing accessors take the
/// [`ProgramScope`] whose tasks to return, since every resident prover program
/// works only on its own tasks.
#[derive(Debug)]
pub struct ProverDbMdbx {
    env: Arc<MdbxEnv>,
}

impl ProverDbMdbx {
    /// Wraps an already-open environment holding the prover tables.
    pub fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }

    /// Opens (or creates) the prover environment at `path`.
    pub fn open(path: &Path, config: &MdbxConfig) -> DbResult<Self> {
        let env = MdbxEnv::open(path, config, &prover_tables()).map_err(to_db_error)?;
        Ok(Self::new(Arc::new(env)))
    }

    /// Row counts of every prover table, as `(table, rows)`, each O(1)
    /// through MDBX's table stat.
    pub fn table_counts(&self) -> MdbxResult<Vec<(&'static str, usize)>> {
        self.env.table_counts(&prover_tables())
    }

    // --- Tasks ---

    /// Reads the task stored under `key`, with its program key.
    pub fn get_task<K: ProverTaskKey>(&self, key: &K) -> DbResult<Option<DBProverTask>> {
        self.env
            .view(|r| r.get::<K::Table>(key))
            .map_err(to_db_error)
    }

    /// Stores a new task, refusing to overwrite one already under `key`.
    ///
    /// Errs with [`DbError::EntryAlreadyExists`] when the key is taken.
    pub fn insert_task<K: ProverTaskKey>(&self, key: &K, task: DBProverTask) -> DbResult<()> {
        let inserted = self
            .env
            .update(|w| {
                if w.get::<K::Table>(key)?.is_some() {
                    return Ok(false);
                }
                w.put::<K::Table>(key, &task)?;
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
    pub fn put_task<K: ProverTaskKey>(&self, key: &K, task: DBProverTask) -> DbResult<()> {
        self.env
            .update(|w| {
                w.put::<K::Table>(key, &task)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    /// Removes the task under `key`, reporting whether one was there.
    pub fn delete_task<K: ProverTaskKey>(&self, key: &K) -> DbResult<bool> {
        self.env
            .update(|w| w.delete::<K::Table>(key))
            .map_err(to_db_error)
    }

    /// Lists the tasks in `scope` that want a rescan and whose retry time has
    /// passed or was never written.
    ///
    /// A rescan status with no `retry_after` is a record whose scheduling
    /// write did not land after the status did. The scanner selects on the
    /// timestamp and startup recovery only takes `Pending` and `Proving`, so
    /// treating it as due is the only thing that ever re-spawns the task.
    pub fn list_retriable_tasks<K: ProverTaskKey>(
        &self,
        scope: &ProgramScope,
        now_secs: u64,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_scoped_tasks_where::<K>(scope, |task| {
            task.status().wants_rescan() && task.retry_after_secs().is_none_or(|t| t <= now_secs)
        })
    }

    /// Lists the tasks in `scope` that have not reached a terminal status.
    pub fn list_unfinished_tasks<K: ProverTaskKey>(
        &self,
        scope: &ProgramScope,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_scoped_tasks_where::<K>(scope, |task| task.status().is_unfinished())
    }

    /// Gives every legacy task of `scope`'s spec version to `scope`'s program,
    /// and returns how many it took. A legacy task was stored before tasks
    /// recorded their program.
    ///
    /// Call this only for a version's only program. With two, nothing says
    /// which of them wrote a legacy task.
    pub fn adopt_legacy_tasks<K: ProverTaskKey>(&self, scope: &ProgramScope) -> DbResult<usize> {
        self.env
            .update(|w| {
                let mut legacy = Vec::new();
                w.for_each::<K::Table>(|key, task| {
                    if key.spec_version() == scope.spec_version() && task.program_key().is_none() {
                        legacy.push((key, task));
                    }
                    Ok(())
                })?;
                let adopted = legacy.len();
                for (key, task) in legacy {
                    w.put::<K::Table>(&key, &scope.adopt(task))?;
                }
                Ok(adopted)
            })
            .map_err(to_db_error)
    }

    /// Counts `K`'s legacy tasks of `spec_version` that have not reached a
    /// terminal status. A legacy task was stored before tasks recorded their
    /// program.
    pub fn count_legacy_open_tasks<K: ProverTaskKey>(
        &self,
        spec_version: AlpenSpecId,
    ) -> DbResult<usize> {
        self.count_tasks_where::<K>(|key, task| {
            key.spec_version() == spec_version
                && task.program_key().is_none()
                && !task.status().is_terminal()
        })
    }

    /// Lists every task in `K`'s table, for every spec version and program.
    pub fn list_all_tasks<K: ProverTaskKey>(&self) -> DbResult<Vec<(K, DBProverTask)>> {
        self.list_tasks_where::<K>(|_, _| true)
    }

    /// Counts `K`'s tasks in `scope`.
    pub fn count_tasks<K: ProverTaskKey>(&self, scope: &ProgramScope) -> DbResult<usize> {
        self.count_tasks_where::<K>(|key, task| {
            key.spec_version() == scope.spec_version() && scope.includes(task)
        })
    }

    /// Lists `K`'s tasks in `scope` that pass `keep`, without their program
    /// key.
    ///
    /// Filters on the stored row, so only the rows it returns pay for
    /// rebuilding the paas record.
    fn list_scoped_tasks_where<K: ProverTaskKey>(
        &self,
        scope: &ProgramScope,
        mut keep: impl FnMut(&DBProverTask) -> bool,
    ) -> DbResult<Vec<(K, TaskRecordData)>> {
        self.list_tasks_where::<K>(|key, task| {
            key.spec_version() == scope.spec_version() && scope.includes(task) && keep(task)
        })?
        .into_iter()
        .map(|(key, task)| {
            let record = task
                .record()
                .map_err(|e| DbError::Other(format!("{}: {e}", <K::Table as Schema>::NAME)))?;
            Ok((key, record))
        })
        .collect()
    }

    fn count_tasks_where<K: ProverTaskKey>(
        &self,
        mut keep: impl FnMut(&K, &DBProverTask) -> bool,
    ) -> DbResult<usize> {
        self.env
            .view(|r| {
                let mut count = 0usize;
                r.for_each::<K::Table>(|key, task| {
                    if keep(&key, &task) {
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
        mut keep: impl FnMut(&K, &DBProverTask) -> bool,
    ) -> DbResult<Vec<(K, DBProverTask)>> {
        self.env
            .view(|r| {
                let mut out = Vec::new();
                r.for_each::<K::Table>(|key, task| {
                    if keep(&key, &task) {
                        out.push((key, task));
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
    use strata_predicate::{PredicateKey, PredicateTypeId};
    use zkaleido::{ProgramId, Proof, ProofMetadata, ProofReceipt, ProofType, PublicValues, ZkVm};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn setup_db() -> ProverDbMdbx {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("ee-mdbx-prover-test-{}-{n}", process::id()));
        ProverDbMdbx::open(&path, &MdbxConfig::small()).unwrap()
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

    fn predicate_key(byte: u8) -> PredicateKey {
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![byte; 32]).unwrap()
    }

    /// The scope of a program of `spec_version`.
    fn program_of(spec_version: AlpenSpecId) -> ProgramScope {
        ProgramScope::new(spec_version, predicate_key(u16::from(spec_version) as u8))
    }

    fn pending() -> TaskRecordData {
        TaskRecordData::new(TaskStatus::Pending)
    }

    #[test]
    fn insert_task_is_idempotent_guarded() {
        let db = setup_db();
        let scope = program_of(AlpenSpecId::V0);
        let key = chunk_key(AlpenSpecId::V0, 1);

        db.insert_task(&key, scope.tag(&pending())).unwrap();
        assert!(db.get_task(&key).unwrap().is_some());
        assert!(matches!(
            db.insert_task(&key, scope.tag(&pending())),
            Err(DbError::EntryAlreadyExists)
        ));

        db.put_task(&key, scope.tag(&pending())).unwrap();
        assert_eq!(db.count_tasks::<ChunkTaskKey>(&scope).unwrap(), 1);

        assert!(db.delete_task(&key).unwrap());
        assert!(!db.delete_task(&key).unwrap());
        assert_eq!(db.count_tasks::<ChunkTaskKey>(&scope).unwrap(), 0);
    }

    #[test]
    fn chunk_and_batch_tasks_over_the_same_range_do_not_collide() {
        let db = setup_db();
        let scope = program_of(AlpenSpecId::V0);
        let chunk = chunk_key(AlpenSpecId::V0, 1);
        let batch = batch_key(AlpenSpecId::V0, 1);
        assert_eq!(chunk.task_bytes(), batch.task_bytes());

        db.insert_task(&chunk, scope.tag(&pending())).unwrap();
        assert!(db.get_task(&batch).unwrap().is_none());
        assert!(db.list_all_tasks::<BatchTaskKey>().unwrap().is_empty());

        db.insert_task(
            &batch,
            scope.tag(&TaskRecordData::new(TaskStatus::Completed)),
        )
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
        let v0_scope = program_of(AlpenSpecId::V0);
        let v1_scope = program_of(AlpenSpecId::V1);
        let v0 = chunk_key(AlpenSpecId::V0, 1);
        let v1 = chunk_key(AlpenSpecId::V1, 1);
        assert_eq!(v0.chunk_id(), v1.chunk_id());

        db.insert_task(&v0, v0_scope.tag(&pending())).unwrap();
        let mut retriable = TaskRecordData::new(TaskStatus::TransientFailure {
            counts: AttemptCounts::default(),
            error: "boom".to_string(),
        });
        retriable.set_retry_after_secs(Some(10));
        db.insert_task(&v1, v1_scope.tag(&retriable)).unwrap();

        // Pending is unfinished; a transient failure is retriable instead.
        let unfinished = db.list_unfinished_tasks::<ChunkTaskKey>(&v0_scope).unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].0, v0);
        assert!(db
            .list_unfinished_tasks::<ChunkTaskKey>(&v1_scope)
            .unwrap()
            .is_empty());

        assert!(db
            .list_retriable_tasks::<ChunkTaskKey>(&v0_scope, 100)
            .unwrap()
            .is_empty());
        let retriable = db
            .list_retriable_tasks::<ChunkTaskKey>(&v1_scope, 100)
            .unwrap();
        assert_eq!(retriable.len(), 1);
        assert_eq!(retriable[0].0, v1);
        assert!(db
            .list_retriable_tasks::<ChunkTaskKey>(&v1_scope, 5)
            .unwrap()
            .is_empty());

        // A transient failure whose retry_after never landed is due at any time.
        let stranded = chunk_key(AlpenSpecId::V1, 2);
        db.insert_task(
            &stranded,
            v1_scope.tag(&TaskRecordData::new(TaskStatus::TransientFailure {
                counts: AttemptCounts::default(),
                error: "retry_after write failed".to_string(),
            })),
        )
        .unwrap();
        let retriable = db
            .list_retriable_tasks::<ChunkTaskKey>(&v1_scope, 0)
            .unwrap();
        assert_eq!(retriable.len(), 1);
        assert_eq!(retriable[0].0, stranded);
        assert_eq!(
            db.list_retriable_tasks::<ChunkTaskKey>(&v1_scope, 100)
                .unwrap()
                .len(),
            2
        );
        assert!(db.delete_task(&stranded).unwrap());

        assert_eq!(db.count_tasks::<ChunkTaskKey>(&v0_scope).unwrap(), 1);
        assert_eq!(db.count_tasks::<ChunkTaskKey>(&v1_scope).unwrap(), 1);
        assert_eq!(db.list_all_tasks::<ChunkTaskKey>().unwrap().len(), 2);
    }

    /// Two programs of one version share the version's rows, so each must
    /// list only the tasks in its scope. Neither scope includes a legacy task.
    #[test]
    fn listings_are_scoped_to_the_program() {
        let db = setup_db();
        let first = ProgramScope::new(AlpenSpecId::V2, predicate_key(1));
        let second = ProgramScope::new(AlpenSpecId::V2, predicate_key(2));

        db.insert_task(&chunk_key(AlpenSpecId::V2, 1), first.tag(&pending()))
            .unwrap();
        db.insert_task(&chunk_key(AlpenSpecId::V2, 3), second.tag(&pending()))
            .unwrap();
        db.insert_task(
            &chunk_key(AlpenSpecId::V2, 5),
            DBProverTask::new(None, &pending()),
        )
        .unwrap();

        let listed = |scope: &ProgramScope| {
            db.list_unfinished_tasks::<ChunkTaskKey>(scope)
                .unwrap()
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(&first), vec![chunk_key(AlpenSpecId::V2, 1)]);
        assert_eq!(listed(&second), vec![chunk_key(AlpenSpecId::V2, 3)]);
    }

    /// Adopting gives the program the keyless rows of its own version only,
    /// and leaves the rest of each row as it was.
    #[test]
    fn adopting_tags_only_the_versions_legacy_tasks() {
        let db = setup_db();
        let scope = ProgramScope::new(AlpenSpecId::V2, predicate_key(1));
        let other = ProgramScope::new(AlpenSpecId::V2, predicate_key(2));
        let legacy = |record: &TaskRecordData| DBProverTask::new(None, record);
        let mut retrying = pending();
        retrying.set_retry_after_secs(Some(42));

        db.insert_task(&chunk_key(AlpenSpecId::V2, 1), legacy(&retrying))
            .unwrap();
        db.insert_task(
            &chunk_key(AlpenSpecId::V2, 3),
            legacy(&TaskRecordData::new(TaskStatus::Completed)),
        )
        .unwrap();
        db.insert_task(&chunk_key(AlpenSpecId::V2, 5), other.tag(&pending()))
            .unwrap();
        db.insert_task(&chunk_key(AlpenSpecId::V1, 7), legacy(&pending()))
            .unwrap();

        assert_eq!(db.adopt_legacy_tasks::<ChunkTaskKey>(&scope).unwrap(), 2);
        assert_eq!(db.adopt_legacy_tasks::<BatchTaskKey>(&scope).unwrap(), 0);

        let adopted = db
            .get_task(&chunk_key(AlpenSpecId::V2, 1))
            .unwrap()
            .unwrap();
        assert_eq!(adopted.program_key(), Some(&predicate_key(1)));
        let record = adopted.record().unwrap();
        assert_eq!(record.updated_at_secs(), retrying.updated_at_secs());
        assert_eq!(record.retry_after_secs(), Some(42));
        assert_eq!(db.count_tasks::<ChunkTaskKey>(&scope).unwrap(), 2);

        let untouched = |key| db.get_task(&key).unwrap().unwrap().program_key().cloned();
        assert_eq!(
            untouched(chunk_key(AlpenSpecId::V2, 5)),
            Some(predicate_key(2))
        );
        assert_eq!(untouched(chunk_key(AlpenSpecId::V1, 7)), None);

        // A second pass finds nothing left to adopt.
        assert_eq!(db.adopt_legacy_tasks::<ChunkTaskKey>(&scope).unwrap(), 0);
    }

    /// Only the unfinished legacy tasks of one version count: those are what
    /// no program can take once two share the version.
    #[test]
    fn legacy_open_tasks_are_counted_per_version() {
        let db = setup_db();
        let tagged = ProgramScope::new(AlpenSpecId::V2, predicate_key(1));
        let legacy = |record: &TaskRecordData| DBProverTask::new(None, record);

        db.insert_task(&chunk_key(AlpenSpecId::V2, 1), legacy(&pending()))
            .unwrap();
        db.insert_task(
            &chunk_key(AlpenSpecId::V2, 3),
            legacy(&TaskRecordData::new(TaskStatus::Completed)),
        )
        .unwrap();
        db.insert_task(&chunk_key(AlpenSpecId::V2, 5), tagged.tag(&pending()))
            .unwrap();
        db.insert_task(&chunk_key(AlpenSpecId::V1, 7), legacy(&pending()))
            .unwrap();

        assert_eq!(
            db.count_legacy_open_tasks::<ChunkTaskKey>(AlpenSpecId::V2)
                .unwrap(),
            1
        );
        assert_eq!(
            db.count_legacy_open_tasks::<BatchTaskKey>(AlpenSpecId::V2)
                .unwrap(),
            0
        );
    }
}
