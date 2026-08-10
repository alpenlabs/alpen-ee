//! MDBX-backed implementation of [`L1BroadcastDatabase`].
//!
//! Mirrors the previous sled store one-for-one, keeping the same Borsh-encoded
//! [`L1TxEntry`] values and the same index/txid table split. Because MDBX
//! serializes writers, the check-and-allocate in [`put_tx_entry`] and the
//! range delete in [`del_tx_entries_from_idx`] each run inside a single atomic
//! `update` closure with no optimistic-retry loop.
//!
//! [`put_tx_entry`]: L1BroadcastDatabase::put_tx_entry
//! [`del_tx_entries_from_idx`]: L1BroadcastDatabase::del_tx_entries_from_idx

use std::sync::Arc;

use alpen_db_store_mdbx::{DbError as MdbxError, MdbxEnv};
use strata_db_types::{
    errors::DbError,
    l1_broadcast::{L1BroadcastDatabase, L1TxEntry},
    DbResult,
};
use strata_identifiers::Buf32;

use super::schema::{L1BroadcastTxIdSchema, L1BroadcastTxSchema};

/// Maps a storage-engine error into the broadcast database error type.
fn map_mdbx(err: MdbxError) -> DbError {
    DbError::IoError(err.to_string())
}

/// MDBX-backed [`L1BroadcastDatabase`] over a shared [`MdbxEnv`].
///
/// The environment may be shared with other DA-pipeline stores; this type uses
/// only the broadcast tables.
#[derive(Debug)]
pub(crate) struct L1BroadcastDbMdbx {
    env: Arc<MdbxEnv>,
}

impl L1BroadcastDbMdbx {
    /// Wraps an already-open environment whose tables include the broadcast
    /// tables.
    pub(crate) fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }
}

impl L1BroadcastDatabase for L1BroadcastDbMdbx {
    fn put_tx_entry(&self, txid: Buf32, txentry: L1TxEntry) -> DbResult<Option<u64>> {
        self.env
            .update(|w| {
                // Allocate a fresh index only for a txid we haven't seen before.
                let idx = if w.get::<L1BroadcastTxSchema>(&txid)?.is_none() {
                    let next = match w.last::<L1BroadcastTxIdSchema>()? {
                        Some((last_idx, _)) => last_idx + 1,
                        None => 0,
                    };
                    w.put::<L1BroadcastTxIdSchema>(&next, &txid)?;
                    Some(next)
                } else {
                    None
                };
                w.put::<L1BroadcastTxSchema>(&txid, &txentry)?;
                Ok(idx)
            })
            .map_err(map_mdbx)
    }

    fn put_tx_entry_by_idx(&self, idx: u64, txentry: L1TxEntry) -> DbResult<()> {
        // The error paths write nothing, so committing the (empty) transaction
        // and translating the sentinel outside the closure is safe.
        enum Outcome {
            Updated,
            MissingIdx,
            MissingTxid,
            Mismatch,
        }

        let outcome = self
            .env
            .update(|w| {
                let Some(txid) = w.get::<L1BroadcastTxIdSchema>(&idx)? else {
                    return Ok(Outcome::MissingIdx);
                };
                let Some(existing) = w.get::<L1BroadcastTxSchema>(&txid)? else {
                    return Ok(Outcome::MissingTxid);
                };
                if existing.tx_raw() != txentry.tx_raw() {
                    return Ok(Outcome::Mismatch);
                }
                w.put::<L1BroadcastTxSchema>(&txid, &txentry)?;
                Ok(Outcome::Updated)
            })
            .map_err(map_mdbx)?;

        match outcome {
            Outcome::Updated => Ok(()),
            Outcome::MissingIdx => Err(DbError::Other(format!(
                "Entry does not exist for idx {idx:?}"
            ))),
            Outcome::MissingTxid => Err(DbError::Other(format!(
                "Entry does not exist for txid at idx {idx:?}"
            ))),
            Outcome::Mismatch => Err(DbError::Other(format!(
                "tx entry at idx {idx:?} cannot be updated with a different transaction"
            ))),
        }
    }

    fn del_tx_entry(&self, txid: Buf32) -> DbResult<bool> {
        self.env
            .update(|w| {
                let exists = w.get::<L1BroadcastTxSchema>(&txid)?.is_some();
                if exists {
                    w.delete::<L1BroadcastTxSchema>(&txid)?;
                }
                Ok(exists)
            })
            .map_err(map_mdbx)
    }

    fn del_tx_entries_from_idx(&self, start_idx: u64) -> DbResult<Vec<u64>> {
        self.env
            .update(|w| {
                let Some((last_idx, _)) = w.last::<L1BroadcastTxIdSchema>()? else {
                    return Ok(Vec::new());
                };
                if start_idx > last_idx {
                    return Ok(Vec::new());
                }

                let mut deleted = Vec::new();
                for idx in start_idx..=last_idx {
                    if let Some(txid) = w.get::<L1BroadcastTxIdSchema>(&idx)? {
                        w.delete::<L1BroadcastTxIdSchema>(&idx)?;
                        w.delete::<L1BroadcastTxSchema>(&txid)?;
                        deleted.push(idx);
                    }
                }
                Ok(deleted)
            })
            .map_err(map_mdbx)
    }

    fn get_tx_entry_by_id(&self, txid: Buf32) -> DbResult<Option<L1TxEntry>> {
        self.env
            .view(|r| r.get::<L1BroadcastTxSchema>(&txid))
            .map_err(map_mdbx)
    }

    fn get_next_tx_idx(&self) -> DbResult<u64> {
        self.env
            .view(|r| {
                Ok(match r.last::<L1BroadcastTxIdSchema>()? {
                    Some((last_idx, _)) => last_idx + 1,
                    None => 0,
                })
            })
            .map_err(map_mdbx)
    }

    fn get_txid(&self, idx: u64) -> DbResult<Option<Buf32>> {
        self.env
            .view(|r| r.get::<L1BroadcastTxIdSchema>(&idx))
            .map_err(map_mdbx)
    }

    fn get_tx_entry(&self, idx: u64) -> DbResult<Option<L1TxEntry>> {
        // `Some(inner)` => the index maps to a txid (`inner` is its entry, if
        // any); `None` => no index mapping, which is an error like the sled impl.
        let resolved = self
            .env
            .view(|r| {
                Ok(match r.get::<L1BroadcastTxIdSchema>(&idx)? {
                    Some(txid) => Some(r.get::<L1BroadcastTxSchema>(&txid)?),
                    None => None,
                })
            })
            .map_err(map_mdbx)?;

        match resolved {
            Some(entry) => Ok(entry),
            None => Err(DbError::Other(format!(
                "Entry does not exist for idx {idx:?}"
            ))),
        }
    }

    fn get_last_tx_entry(&self) -> DbResult<Option<L1TxEntry>> {
        self.env
            .view(|r| Ok(r.last::<L1BroadcastTxSchema>()?.map(|(_, entry)| entry)))
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
    use strata_db_tests::l1_broadcast_db_tests;

    use super::*;
    use crate::mdbxdb::schema::da_tables;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_env() -> Arc<MdbxEnv> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path: PathBuf = env::temp_dir();
        path.push(format!("ee-mdbx-broadcast-test-{}-{n}", process::id()));
        Arc::new(MdbxEnv::open(&path, &MdbxConfig::small(), &da_tables()).unwrap())
    }

    fn setup_db() -> L1BroadcastDbMdbx {
        L1BroadcastDbMdbx::new(temp_env())
    }

    l1_broadcast_db_tests!(setup_db());
}
