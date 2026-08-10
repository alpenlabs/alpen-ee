//! MDBX-backed implementation of [`L1ChunkedEnvelopeDatabase`].
//!
//! A single index-keyed table of Borsh-encoded
//! [`ChunkedEnvelopeEntry`] values. The range delete runs inside one atomic
//! `update` closure under the single MDBX writer.

use std::sync::Arc;

use alpen_db_store_mdbx::{DbError as MdbxError, MdbxEnv};
use strata_db_types::{
    chunked_envelope::{ChunkedEnvelopeEntry, L1ChunkedEnvelopeDatabase},
    errors::DbError,
    DbResult,
};

use super::schema::L1ChunkedEnvelopeSchema;

/// Maps a storage-engine error into the chunked-envelope database error type.
fn map_mdbx(err: MdbxError) -> DbError {
    DbError::IoError(err.to_string())
}

/// MDBX-backed [`L1ChunkedEnvelopeDatabase`] over a shared [`MdbxEnv`].
#[derive(Debug)]
pub(crate) struct L1ChunkedEnvelopeDbMdbx {
    env: Arc<MdbxEnv>,
}

impl L1ChunkedEnvelopeDbMdbx {
    /// Wraps an already-open environment whose tables include the chunked
    /// envelope table.
    pub(crate) fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }
}

impl L1ChunkedEnvelopeDatabase for L1ChunkedEnvelopeDbMdbx {
    fn put_chunked_envelope_entry(&self, idx: u64, entry: ChunkedEnvelopeEntry) -> DbResult<()> {
        self.env
            .update(|w| w.put::<L1ChunkedEnvelopeSchema>(&idx, &entry))
            .map_err(map_mdbx)
    }

    fn get_chunked_envelope_entry(&self, idx: u64) -> DbResult<Option<ChunkedEnvelopeEntry>> {
        self.env
            .view(|r| r.get::<L1ChunkedEnvelopeSchema>(&idx))
            .map_err(map_mdbx)
    }

    fn get_chunked_envelope_entries_from(
        &self,
        start_idx: u64,
        max_count: usize,
    ) -> DbResult<Vec<(u64, ChunkedEnvelopeEntry)>> {
        // Big-endian integer keys make `for_each` ascending, so collecting every
        // entry at or beyond `start_idx` and truncating yields the lowest
        // `max_count` in order. The table only holds unfinalized entries, so it
        // stays small.
        self.env
            .view(|r| {
                let mut entries = Vec::new();
                r.for_each::<L1ChunkedEnvelopeSchema>(|idx, entry| {
                    if idx >= start_idx {
                        entries.push((idx, entry));
                    }
                    Ok(())
                })?;
                entries.truncate(max_count);
                Ok(entries)
            })
            .map_err(map_mdbx)
    }

    fn get_next_chunked_envelope_idx(&self) -> DbResult<u64> {
        self.env
            .view(|r| {
                Ok(r.last::<L1ChunkedEnvelopeSchema>()?
                    .map(|(idx, _)| idx + 1)
                    .unwrap_or(0))
            })
            .map_err(map_mdbx)
    }

    fn del_chunked_envelope_entry(&self, idx: u64) -> DbResult<bool> {
        self.env
            .update(|w| {
                let exists = w.get::<L1ChunkedEnvelopeSchema>(&idx)?.is_some();
                if exists {
                    w.delete::<L1ChunkedEnvelopeSchema>(&idx)?;
                }
                Ok(exists)
            })
            .map_err(map_mdbx)
    }

    fn del_chunked_envelope_entries_from_idx(&self, start_idx: u64) -> DbResult<Vec<u64>> {
        self.env
            .update(|w| {
                let Some((last_idx, _)) = w.last::<L1ChunkedEnvelopeSchema>()? else {
                    return Ok(Vec::new());
                };
                if start_idx > last_idx {
                    return Ok(Vec::new());
                }

                let mut deleted = Vec::new();
                for idx in start_idx..=last_idx {
                    if w.get::<L1ChunkedEnvelopeSchema>(&idx)?.is_some() {
                        w.delete::<L1ChunkedEnvelopeSchema>(&idx)?;
                        deleted.push(idx);
                    }
                }
                Ok(deleted)
            })
            .map_err(map_mdbx)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use alpen_db_store_mdbx::MdbxConfig;
    use strata_db_tests::l1_chunked_envelope_db_tests;

    use super::*;
    use crate::mdbxdb::schema::da_tables;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_env() -> Arc<MdbxEnv> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path: PathBuf = env::temp_dir();
        path.push(format!("ee-mdbx-envelope-test-{}-{n}", process::id()));
        Arc::new(MdbxEnv::open(&path, &MdbxConfig::small(), &da_tables()).unwrap())
    }

    fn setup_db() -> L1ChunkedEnvelopeDbMdbx {
        L1ChunkedEnvelopeDbMdbx::new(temp_env())
    }

    l1_chunked_envelope_db_tests!(setup_db());
}
