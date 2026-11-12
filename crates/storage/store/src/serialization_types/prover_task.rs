//! The stored form of a prover task, and which program proves it.

use alpen_params::AlpenSpecId;
use alpen_storage_mdbx::{CodecError, UpConvert, UpgradeCtx};
use serde::{Deserialize, Serialize};
use strata_paas::{TaskRecordData, TaskStatus};
use strata_predicate::PredicateKey;

/// A prover task as stored: the paas record, and the key of the prover
/// program that proves the task.
///
/// One spec version can have several programs, because a guest fix that keeps
/// the rules gets a new key, not a new version. Each program runs its own paas
/// prover over its version's rows, and paas re-spawns every unfinished task its
/// store lists. So each row records its program's key, and a program only sees
/// the rows in its [`ProgramScope`]. Proving another program's task would sign
/// it with a key the OL doesn't check that batch against.
///
/// The record's fields are stored beside `program_key` rather than nested, so
/// the operator console still edits `status` as a top-level field.
/// `serde(flatten)` would do the same, but the console's reflection can't feed
/// an enum through it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DBProverTask {
    /// Account predicate key of the program that proves the task, in its text
    /// form. `None` for a legacy task, stored before tasks recorded their
    /// program. No program works on a legacy task until it is adopted, which
    /// gives it a key.
    #[serde(with = "predicate_key_text")]
    program_key: Option<PredicateKey>,
    status: TaskStatus,
    updated_at_secs: u64,
    retry_after_secs: Option<u64>,
    #[serde(with = "serde_bytes")]
    metadata: Option<Vec<u8>>,
}

/// The fields [`TaskRecordData`] deserializes from. It keeps them private and
/// stamps "now" in its constructor, so this is how a record with a stored
/// timestamp is rebuilt.
#[derive(Serialize)]
struct TaskRecordShape<'a> {
    status: &'a TaskStatus,
    updated_at_secs: u64,
    retry_after_secs: Option<u64>,
    #[serde(with = "serde_bytes")]
    metadata: Option<Vec<u8>>,
}

impl DBProverTask {
    /// The stored form of `record`, proved by the program with key
    /// `program_key`.
    pub fn new(program_key: Option<PredicateKey>, record: &TaskRecordData) -> Self {
        Self {
            program_key,
            status: record.status().clone(),
            updated_at_secs: record.updated_at_secs(),
            retry_after_secs: record.retry_after_secs(),
            metadata: record.metadata().map(<[u8]>::to_vec),
        }
    }

    /// Account predicate key of the program that proves the task, unless the
    /// task is legacy.
    pub fn program_key(&self) -> Option<&PredicateKey> {
        self.program_key.as_ref()
    }

    /// The task's status.
    pub fn status(&self) -> &TaskStatus {
        &self.status
    }

    /// When the task is next due for a retry, if one was scheduled.
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.retry_after_secs
    }

    /// Rebuilds the paas record.
    pub fn record(&self) -> Result<TaskRecordData, CodecError> {
        let shape = TaskRecordShape {
            status: &self.status,
            updated_at_secs: self.updated_at_secs,
            retry_after_secs: self.retry_after_secs,
            metadata: self.metadata.clone(),
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&shape, &mut cbor)
            .map_err(|e| CodecError::encode("DBProverTask", e.to_string()))?;
        ciborium::from_reader(cbor.as_slice())
            .map_err(|e| CodecError::decode("DBProverTask", e.to_string()))
    }
}

/// Version 1 stored the bare paas record, so its tasks are legacy.
impl UpConvert<DBProverTask> for TaskRecordData {
    fn up_convert(self, _ctx: &UpgradeCtx<'_>) -> Result<DBProverTask, CodecError> {
        Ok(DBProverTask::new(None, &self))
    }
}

/// One prover program: the spec version it proves and its account predicate
/// key.
///
/// It also picks the tasks the program works on: those of its spec version
/// that record its key.
#[derive(Debug, Clone)]
pub struct ProgramScope {
    spec_version: AlpenSpecId,
    key: PredicateKey,
}

impl ProgramScope {
    /// The scope of the program with account predicate `key`, proving
    /// `spec_version`.
    pub fn new(spec_version: AlpenSpecId, key: PredicateKey) -> Self {
        Self { spec_version, key }
    }

    /// The spec version the program proves.
    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }

    /// The program's account predicate key.
    pub fn key(&self) -> &PredicateKey {
        &self.key
    }

    /// Whether `task`, stored under the program's version, is in its scope.
    pub fn includes(&self, task: &DBProverTask) -> bool {
        task.program_key() == Some(&self.key)
    }

    /// The stored form of `record`, tagged with this program's key.
    pub fn tag(&self, record: &TaskRecordData) -> DBProverTask {
        DBProverTask::new(Some(self.key.clone()), record)
    }

    /// `task` with this program's key, the rest of the row unchanged.
    pub(crate) fn adopt(&self, task: DBProverTask) -> DBProverTask {
        DBProverTask {
            program_key: Some(self.key.clone()),
            ..task
        }
    }
}

/// Serde for an optional [`PredicateKey`] as its text form, such as
/// `"Bip340Schnorr:4d4b…"`, in every format. Its own impl writes a binary
/// tuple to a binary format, which the console would render as a list of
/// numbers.
mod predicate_key_text {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};
    use strata_predicate::PredicateKey;

    pub(super) fn serialize<S: Serializer>(
        key: &Option<PredicateKey>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match key {
            Some(key) => ser.collect_str(key),
            None => ser.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<PredicateKey>, D::Error> {
        Option::<String>::deserialize(de)?
            .map(|text| text.parse().map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use alpen_storage_mdbx::ValueCodec;
    use strata_predicate::PredicateTypeId;

    use super::*;
    use crate::mdbxdb::schema::ChunkProverTaskSchema;

    fn key(byte: u8) -> PredicateKey {
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![byte; 32]).unwrap()
    }

    fn record_with_metadata(metadata: Vec<u8>) -> TaskRecordData {
        let mut record = TaskRecordData::new(TaskStatus::Pending);
        record.set_metadata(Some(metadata));
        record
    }

    #[test]
    fn a_version_one_task_decodes_as_legacy() {
        let record = record_with_metadata(vec![1, 2, 3]);
        let mut v1 = vec![1u8];
        ciborium::into_writer(&record, &mut v1).unwrap();

        let task = <DBProverTask as ValueCodec<ChunkProverTaskSchema>>::decode_value(
            &v1,
            &UpgradeCtx::detached(),
        )
        .unwrap();

        assert!(task.program_key().is_none());
        let decoded = task.record().unwrap();
        assert_eq!(decoded.status(), &TaskStatus::Pending);
        assert_eq!(decoded.updated_at_secs(), record.updated_at_secs());
        assert_eq!(decoded.metadata(), Some(&[1u8, 2, 3][..]));
    }

    #[test]
    fn a_tagged_task_round_trips() {
        let mut record = record_with_metadata(vec![7]);
        record.set_retry_after_secs(Some(99));
        let task = ProgramScope::new(AlpenSpecId::V2, key(1)).tag(&record);

        let bytes =
            <DBProverTask as ValueCodec<ChunkProverTaskSchema>>::encode_value(&task).unwrap();
        let decoded = <DBProverTask as ValueCodec<ChunkProverTaskSchema>>::decode_value(
            &bytes,
            &UpgradeCtx::detached(),
        )
        .unwrap();

        assert_eq!(decoded.program_key(), Some(&key(1)));
        let decoded = decoded.record().unwrap();
        assert_eq!(decoded.updated_at_secs(), record.updated_at_secs());
        assert_eq!(decoded.retry_after_secs(), Some(99));
        assert_eq!(decoded.metadata(), Some(&[7u8][..]));
    }

    #[test]
    fn a_scope_includes_only_its_own_tasks() {
        let record = TaskRecordData::new(TaskStatus::Pending);
        let scope = ProgramScope::new(AlpenSpecId::V2, key(1));
        let mine = scope.tag(&record);
        let theirs = ProgramScope::new(AlpenSpecId::V2, key(2)).tag(&record);
        let legacy = DBProverTask::new(None, &record);

        assert!(scope.includes(&mine));
        assert!(!scope.includes(&theirs));
        assert!(!scope.includes(&legacy));
        assert!(scope.includes(&scope.adopt(legacy)));
    }
}
