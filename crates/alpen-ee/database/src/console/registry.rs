//! The table registry: per-table reflection through the production codecs.
//!
//! Each console-visible table implements [`TableReflect`], which knows how to
//! count, fetch, and scan it with the value decoded exactly as the node decodes
//! it. This keeps the console on the single canonical codec path (the design's
//! core invariant) instead of a drifting hand-rolled decoder.
//!
//! Scaffold status: only [`ProverTaskSchema`] is fully reflected. The remaining
//! prover-env tables are registered for inventory (`.tables`/`.schema`) and
//! support `count`, but their `get`/`scan` return a clear "not yet implemented"
//! error. Adding a table is: re-export its schema, add a `TableReflect` impl,
//! and list it in [`prover_env_tables`].

use std::{fmt, marker::PhantomData};

use alpen_db_store_mdbx::{DbError as StoreDbError, Reader, Schema};
use eyre::{bail, eyre};
use strata_paas::{TaskRecordData, TaskStatus};

use super::{
    class::TableClass,
    row::{hex, parse_hex, FieldValue, Row},
};
use crate::mdbxdb::{
    AcctProofIdIndexSchema, AcctProofReceiptSchema, ChunkProofReceiptSchema, ProverTaskSchema,
};

/// Static description of a table, shown by `.tables` and `.schema`.
#[derive(Clone, Copy, Debug)]
pub struct TableInfo {
    /// The MDBX sub-database name (matches `Schema::NAME`).
    pub name: &'static str,
    /// The restoration class.
    pub class: TableClass,
    /// Human description of the key type.
    pub key_desc: &'static str,
    /// Human description of the value shape.
    pub value_desc: &'static str,
}

/// A table's console reflection.
///
/// Methods take a [`Reader`] borrowed from a short read transaction opened by
/// [`ConsoleDb`](super::ConsoleDb), so the read-txn discipline stays owned by
/// the caller.
pub trait TableReflect: fmt::Debug + Send + Sync {
    /// Static metadata for `.tables`/`.schema`.
    fn info(&self) -> TableInfo;

    /// The number of entries (O(1) via MDBX stat).
    fn count(&self, reader: &Reader<'_>) -> eyre::Result<usize>;

    /// Fetches one row by its textual key (hex for byte/hash keys), or `None`.
    fn get(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<Option<Row>>;

    /// Scans the table, returning `(key-hex, row)` for rows where `pred` is
    /// true. The decode loop stays native; only matched rows are collected.
    fn scan(
        &self,
        reader: &Reader<'_>,
        pred: &mut dyn FnMut(&Row) -> eyre::Result<bool>,
    ) -> eyre::Result<Vec<(String, Row)>>;
}

/// Inventory-only reflection: `count` works, `get`/`scan` are not yet wired.
struct CountOnly<S: Schema> {
    info: TableInfo,
    _schema: PhantomData<fn() -> S>,
}

impl<S: Schema> CountOnly<S> {
    fn new(info: TableInfo) -> Self {
        Self {
            info,
            _schema: PhantomData,
        }
    }
}

impl<S: Schema> fmt::Debug for CountOnly<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountOnly")
            .field("name", &self.info.name)
            .finish()
    }
}

impl<S: Schema> TableReflect for CountOnly<S> {
    fn info(&self) -> TableInfo {
        self.info
    }

    fn count(&self, reader: &Reader<'_>) -> eyre::Result<usize> {
        Ok(reader.count::<S>()?)
    }

    fn get(&self, _reader: &Reader<'_>, _key: &str) -> eyre::Result<Option<Row>> {
        bail!(
            "row reflection for `{}` is not implemented in this console scaffold \
             (only `ProverTaskSchema` is reflected so far)",
            self.info.name
        )
    }

    fn scan(
        &self,
        _reader: &Reader<'_>,
        _pred: &mut dyn FnMut(&Row) -> eyre::Result<bool>,
    ) -> eyre::Result<Vec<(String, Row)>> {
        bail!(
            "scan of `{}` is not implemented in this console scaffold \
             (only `ProverTaskSchema` is reflected so far)",
            self.info.name
        )
    }
}

/// Full reflection for the shared prover task store (Class O — the console's
/// first, deliberately un-blocked target: replayable from the prover, no
/// Tier-1 dependency).
#[derive(Debug)]
struct ProverTasks;

impl TableReflect for ProverTasks {
    fn info(&self) -> TableInfo {
        TableInfo {
            name: "ProverTaskSchema",
            class: TableClass::Operational,
            key_desc: "tag-prefixed ProofSpec::Task bytes ([u8] -> hex)",
            value_desc: "{ status: enum, retry_count: u32?, updated_at_secs: u64, \
                          retry_after_secs: u64?, error: str? }",
        }
    }

    fn count(&self, reader: &Reader<'_>) -> eyre::Result<usize> {
        Ok(reader.count::<ProverTaskSchema>()?)
    }

    fn get(&self, reader: &Reader<'_>, key: &str) -> eyre::Result<Option<Row>> {
        let key = parse_hex(key).map_err(|e| eyre!("bad key: {e}"))?;
        let record = reader.get::<ProverTaskSchema>(&key)?;
        Ok(record.map(|data| task_row(&key, &data)))
    }

    fn scan(
        &self,
        reader: &Reader<'_>,
        pred: &mut dyn FnMut(&Row) -> eyre::Result<bool>,
    ) -> eyre::Result<Vec<(String, Row)>> {
        let mut matched = Vec::new();
        let mut pred_err: Option<eyre::Error> = None;

        let scan = reader.for_each::<ProverTaskSchema>(|key, data| {
            let row = task_row(&key, &data);
            match pred(&row) {
                Ok(true) => {
                    matched.push((hex(&key), row));
                    Ok(())
                }
                Ok(false) => Ok(()),
                Err(err) => {
                    // Abort the native iteration by surfacing an error; the real
                    // predicate error is stashed and returned below.
                    pred_err = Some(err);
                    Err(StoreDbError::Env("predicate aborted scan".to_owned()))
                }
            }
        });

        if let Some(err) = pred_err {
            return Err(err);
        }
        scan?;
        Ok(matched)
    }
}

/// Reflects a [`TaskRecordData`] into a [`Row`], flattening the status enum's
/// payload (attempt counters, block reason, error) into scalar fields that read
/// cleanly in a predicate (`t.status == "TransientFailure"`, `t.retry > 30`).
///
/// The three [`AttemptCounts`](strata_paas::AttemptCounts) budgets ride as separate always-present
/// fields rather than one nested value: a status that carries no counters reports
/// zeros, so a predicate can compare them without a null guard.
fn task_row(key: &[u8], data: &TaskRecordData) -> Row {
    let (status, blocked_reason, error) = match data.status() {
        TaskStatus::Pending => ("Pending", None, None),
        TaskStatus::Proving { .. } => ("Proving", None, None),
        TaskStatus::Completed => ("Completed", None, None),
        TaskStatus::Blocked { reason, .. } => ("Blocked", Some(reason.clone()), None),
        TaskStatus::TransientFailure { error, .. } => {
            ("TransientFailure", None, Some(error.clone()))
        }
        TaskStatus::PermanentFailure { error } => ("PermanentFailure", None, Some(error.clone())),
    };
    let counts = data.status().counts();

    Row::new()
        .field("id", FieldValue::Bytes(key.to_vec()))
        .field("status", FieldValue::Enum(status.to_owned()))
        .field("retry", FieldValue::U32(counts.retry))
        .field("resubmit", FieldValue::U32(counts.resubmit))
        .field("recheck", FieldValue::U32(counts.recheck))
        .field("updated_at_secs", FieldValue::U64(data.updated_at_secs()))
        .field(
            "retry_after_secs",
            data.retry_after_secs()
                .map_or(FieldValue::Null, FieldValue::U64),
        )
        .field(
            "blocked_reason",
            blocked_reason.map_or(FieldValue::Null, FieldValue::Str),
        )
        .field("error", error.map_or(FieldValue::Null, FieldValue::Str))
}

/// Builds the console's view of the prover environment's tables.
pub(crate) fn prover_env_tables() -> Vec<Box<dyn TableReflect>> {
    vec![
        Box::new(ProverTasks),
        Box::new(CountOnly::<ChunkProofReceiptSchema>::new(TableInfo {
            name: "ChunkProofReceiptSchema",
            class: TableClass::ProvingCache,
            key_desc: "chunk task bytes ([u8] -> hex)",
            value_desc: "ProofReceiptWithMetadata",
        })),
        Box::new(CountOnly::<AcctProofReceiptSchema>::new(TableInfo {
            name: "AcctProofReceiptSchema",
            class: TableClass::ProvingCache,
            key_desc: "DBBatchId",
            value_desc: "ProofReceiptWithMetadata",
        })),
        Box::new(CountOnly::<AcctProofIdIndexSchema>::new(TableInfo {
            name: "AcctProofIdIndexSchema",
            class: TableClass::Index,
            key_desc: "ProofId (Hash)",
            value_desc: "DBBatchId",
        })),
    ]
}
