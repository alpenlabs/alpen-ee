//! MDBX-backed implementation of [`L1BroadcastDatabase`].
//!
//! Keeps the same Borsh-encoded
//! [`L1TxEntry`] values and the same index/txid table split. Because MDBX
//! serializes writers, the check-and-allocate in [`put_tx_entry`] and the
//! range delete in [`del_tx_entries_from_idx`] each run inside a single atomic
//! `update` closure with no optimistic-retry loop.
//!
//! [`put_tx_entry`]: L1BroadcastDatabase::put_tx_entry
//! [`del_tx_entries_from_idx`]: L1BroadcastDatabase::del_tx_entries_from_idx

use std::sync::Arc;

use alpen_db_store_mdbx::{DbError as MdbxError, MdbxEnv, Writer};
use strata_db_types::{
    common::L1TxId,
    errors::DbError,
    fee_bump::{TxNodeId, TxNodeRecord},
    l1_broadcast::{L1BroadcastDatabase, L1TxEntry, L1TxStatus},
    DbResult,
};
use strata_identifiers::Buf32;

use super::{
    schema::{
        L1BroadcastActiveTxNodeSchema, L1BroadcastTxIdSchema, L1BroadcastTxNodeSchema,
        L1BroadcastTxSchema,
    },
    to_db_error,
};

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
            .map_err(to_db_error)
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
                // `Replaced` is terminal for a txid: a concurrent fee bump has
                // already superseded it, so refuse to move the status back.
                if matches!(existing.status, L1TxStatus::Replaced { .. })
                    && !matches!(txentry.status, L1TxStatus::Replaced { .. })
                {
                    return Ok(Outcome::Updated);
                }
                w.put::<L1BroadcastTxSchema>(&txid, &txentry)?;
                Ok(Outcome::Updated)
            })
            .map_err(to_db_error)?;

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
            .map_err(to_db_error)
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
            .map_err(to_db_error)
    }

    fn get_tx_entry_by_id(&self, txid: Buf32) -> DbResult<Option<L1TxEntry>> {
        self.env
            .view(|r| r.get::<L1BroadcastTxSchema>(&txid))
            .map_err(to_db_error)
    }

    fn get_next_tx_idx(&self) -> DbResult<u64> {
        self.env
            .view(|r| {
                Ok(match r.last::<L1BroadcastTxIdSchema>()? {
                    Some((last_idx, _)) => last_idx + 1,
                    None => 0,
                })
            })
            .map_err(to_db_error)
    }

    fn get_txid(&self, idx: u64) -> DbResult<Option<Buf32>> {
        self.env
            .view(|r| r.get::<L1BroadcastTxIdSchema>(&idx))
            .map_err(to_db_error)
    }

    fn get_tx_entry(&self, idx: u64) -> DbResult<Option<L1TxEntry>> {
        // `Some(inner)` => the index maps to a txid (`inner` is its entry, if
        // any); `None` => no index mapping, which is an error.
        let resolved = self
            .env
            .view(|r| {
                Ok(match r.get::<L1BroadcastTxIdSchema>(&idx)? {
                    Some(txid) => Some(r.get::<L1BroadcastTxSchema>(&txid)?),
                    None => None,
                })
            })
            .map_err(to_db_error)?;

        match resolved {
            Some(entry) => Ok(entry),
            None => Err(DbError::Other(format!(
                "Entry does not exist for idx {idx:?}"
            ))),
        }
    }

    fn get_last_tx_entry(&self) -> DbResult<Option<L1TxEntry>> {
        // Resolve through the index table: `L1BroadcastTxSchema` is keyed by
        // txid, so its last entry is the greatest txid, not the most recently
        // inserted one.
        self.env
            .view(|r| {
                let Some((_, txid)) = r.last::<L1BroadcastTxIdSchema>()? else {
                    return Ok(None);
                };
                r.get::<L1BroadcastTxSchema>(&txid)
            })
            .map_err(to_db_error)
    }

    fn put_replacement_tx_entry(
        &self,
        original_txid: Buf32,
        replacement_txid: Buf32,
        replacement: L1TxEntry,
    ) -> DbResult<Option<u64>> {
        self.env
            .update(|w| {
                let Some(mut original) = w.get::<L1BroadcastTxSchema>(&original_txid)? else {
                    return Ok(None);
                };
                if !is_bumpable(&original.status) {
                    return Ok(None);
                }

                // The swap is all-or-nothing, and `None` tells the caller
                // nothing was written. An already-present replacement row would
                // break that contract: there would be no index to report yet
                // the original would still be transitioned. It also cannot come
                // from a completed swap, since insert and transition commit
                // together.
                if w.get::<L1BroadcastTxSchema>(&replacement_txid)?.is_some() {
                    return Ok(None);
                }

                let idx = next_tx_idx(w)?;
                w.put::<L1BroadcastTxIdSchema>(&idx, &replacement_txid)?;

                // The reverse link is written here so it can never disagree
                // with the forward one.
                let mut replacement = replacement.clone();
                replacement.set_replaces(L1TxId::from(original_txid.0));
                w.put::<L1BroadcastTxSchema>(&replacement_txid, &replacement)?;

                original.status = L1TxStatus::Replaced {
                    by: L1TxId::from(replacement_txid.0),
                };
                w.put::<L1BroadcastTxSchema>(&original_txid, &original)?;

                Ok(Some(idx))
            })
            .map_err(to_db_error)
    }

    fn try_mark_tx_entry_replaced(&self, txid: Buf32, replacement_txid: L1TxId) -> DbResult<bool> {
        self.env
            .update(|w| {
                let Some(mut entry) = w.get::<L1BroadcastTxSchema>(&txid)? else {
                    return Ok(false);
                };
                if !is_bumpable(&entry.status) {
                    return Ok(false);
                }
                entry.status = L1TxStatus::Replaced {
                    by: replacement_txid,
                };
                w.put::<L1BroadcastTxSchema>(&txid, &entry)?;
                Ok(true)
            })
            .map_err(to_db_error)
    }

    fn adopt_confirmed_ancestor(
        &self,
        loser_txid: Buf32,
        winner_txid: Buf32,
        winner_status: L1TxStatus,
    ) -> DbResult<bool> {
        self.env
            .update(|w| {
                let (Some(mut loser), Some(mut winner)) = (
                    w.get::<L1BroadcastTxSchema>(&loser_txid)?,
                    w.get::<L1BroadcastTxSchema>(&winner_txid)?,
                ) else {
                    return Ok(false);
                };

                // A loser that has already left the bumpable states was
                // superseded by a concurrent replacement write. Reversing over
                // it would cut that replacement out of the chain while it stays
                // indexed and broadcastable, so the chain head would report the
                // older ancestor while a live transaction spent the same
                // inputs.
                if !is_bumpable(&loser.status) {
                    return Ok(false);
                }

                // Only reverse a link this chain actually has. Without the
                // check a stale caller could point two unrelated entries at
                // each other.
                if !replacement_chain_reaches(w, &winner.status, loser_txid)? {
                    return Ok(false);
                }

                winner.status = winner_status.clone();
                loser.status = L1TxStatus::Replaced {
                    by: L1TxId::from(winner_txid.0),
                };
                w.put::<L1BroadcastTxSchema>(&winner_txid, &winner)?;
                w.put::<L1BroadcastTxSchema>(&loser_txid, &loser)?;

                Ok(true)
            })
            .map_err(to_db_error)
    }

    fn put_tx_node(&self, node_id: TxNodeId, record: TxNodeRecord) -> DbResult<()> {
        // Record and active-set membership commit together so a crash cannot
        // leave a live chain outside the set the replacement pass scans.
        self.env
            .update(|w| {
                w.put::<L1BroadcastTxNodeSchema>(&node_id, &record)?;
                if record.terminal_error.is_some() {
                    w.delete::<L1BroadcastActiveTxNodeSchema>(&node_id)?;
                } else {
                    w.put::<L1BroadcastActiveTxNodeSchema>(&node_id, &())?;
                }
                Ok(())
            })
            .map_err(to_db_error)
    }

    fn get_tx_node(&self, node_id: TxNodeId) -> DbResult<Option<TxNodeRecord>> {
        self.env
            .view(|r| r.get::<L1BroadcastTxNodeSchema>(&node_id))
            .map_err(to_db_error)
    }

    fn get_all_tx_nodes(&self) -> DbResult<Vec<TxNodeRecord>> {
        self.env
            .view(|r| {
                let mut records = Vec::new();
                r.for_each::<L1BroadcastTxNodeSchema>(|_, record| {
                    records.push(record);
                    Ok(())
                })?;
                Ok(records)
            })
            .map_err(to_db_error)
    }

    fn get_active_tx_nodes(&self) -> DbResult<Vec<TxNodeRecord>> {
        self.env
            .view(|r| {
                let mut node_ids = Vec::new();
                r.for_each::<L1BroadcastActiveTxNodeSchema>(|node_id, ()| {
                    node_ids.push(node_id);
                    Ok(())
                })?;

                let mut records = Vec::new();
                for node_id in node_ids {
                    if let Some(record) = r.get::<L1BroadcastTxNodeSchema>(&node_id)? {
                        records.push(record);
                    }
                }
                Ok(records)
            })
            .map_err(to_db_error)
    }

    fn retire_tx_node(&self, node_id: TxNodeId, expected_active_txid: L1TxId) -> DbResult<bool> {
        self.env
            .update(|w| {
                let Some(mut record) = w.get::<L1BroadcastTxNodeSchema>(&node_id)? else {
                    // A membership entry without a record indexes nothing; drop it.
                    w.delete::<L1BroadcastActiveTxNodeSchema>(&node_id)?;
                    return Ok(false);
                };
                if record.active_txid != expected_active_txid {
                    return Ok(false);
                }
                // The record is kept forever for point lookups, but a retired
                // chain never rebroadcasts or re-signs, so its raw transaction
                // bytes are dead weight. Dropping them bounds the permanent
                // record to metadata size.
                record.forget_all_raw_txs();
                w.put::<L1BroadcastTxNodeSchema>(&node_id, &record)?;
                w.delete::<L1BroadcastActiveTxNodeSchema>(&node_id)?;
                Ok(true)
            })
            .map_err(to_db_error)
    }
}

/// Reports whether an entry in `status` can still be superseded by a fee bump.
fn is_bumpable(status: &L1TxStatus) -> bool {
    matches!(status, L1TxStatus::Unpublished | L1TxStatus::Published)
}

/// Returns the next free broadcast index.
fn next_tx_idx(w: &Writer<'_>) -> Result<u64, MdbxError> {
    Ok(match w.last::<L1BroadcastTxIdSchema>()? {
        Some((last_idx, _)) => last_idx + 1,
        None => 0,
    })
}

/// Bound on how far the adoption check walks a replacement chain.
///
/// Comfortably above the hop budget the broadcaster's ancestor search uses, so
/// the check never refuses a pair that search was able to find.
const MAX_ADOPTION_CHAIN_HOPS: usize = 64;

/// Reports whether following `status`'s forward [`L1TxStatus::Replaced`] links
/// arrives at `target`.
///
/// The winner of an adoption need not be the loser's immediate parent. A chain
/// bumped more than once has intermediate attempts between them, and the miner
/// is free to include any of them, so requiring a direct link would refuse every
/// adoption in a chain longer than two.
fn replacement_chain_reaches(
    w: &Writer<'_>,
    status: &L1TxStatus,
    target: Buf32,
) -> Result<bool, MdbxError> {
    let L1TxStatus::Replaced { by } = status else {
        return Ok(false);
    };
    let mut current = Buf32(by.0);

    for _ in 0..MAX_ADOPTION_CHAIN_HOPS {
        if current == target {
            return Ok(true);
        }
        let Some(entry) = w.get::<L1BroadcastTxSchema>(&current)? else {
            return Ok(false);
        };
        let L1TxStatus::Replaced { by } = entry.status else {
            return Ok(false);
        };
        current = Buf32(by.0);
    }

    Ok(false)
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
