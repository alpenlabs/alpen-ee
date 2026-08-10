use std::{path::Path, sync::Arc};

use alpen_db_store_mdbx::{MdbxConfig, MdbxEnv};
use alpen_ee_common::{
    AccessedStateRecord, Batch, BatchId, BatchStatus, Chunk, ChunkId, ChunkStatus,
    EeAccountStateAtEpoch, ExecBlockRecord,
};
use strata_acct_types::Hash;
use strata_ee_acct_types::EeAccountState;
use strata_identifiers::{EpochCommitment, OLBlockId};
use tracing::{error, trace, warn};

use super::schema::{
    node_tables, AccountStateAtOLEpochSchema, BatchByIdxSchema, BatchChunksSchema,
    BatchIdToIdxSchema, BlockAccessedStateSchema, BlockWitnessSchema, BytecodeSchema,
    ChunkByIdxSchema, ChunkIdToIdxSchema, ExecBlockFinalizedSchema, ExecBlockPayloadSchema,
    ExecBlockSchema, ExecBlocksAtHeightSchema, OLBlockAtEpochSchema,
};
use crate::{
    database::EeNodeDb,
    serialization_types::{
        DBAccountStateAtEpoch, DBBatchId, DBBatchWithStatus, DBChunkId, DBChunkWithStatus,
        DBExecBlockRecord, DBOLBlockId,
    },
    DbError, DbResult,
};

/// MDBX-backed `EeNodeDb` over a single [`MdbxEnv`].
///
/// The environment may be shared with other EE stores (one write-lock, atomic
/// cross-table commits); this type registers and uses only the node tables.
#[derive(Debug)]
pub struct EeNodeDbMdbx {
    env: Arc<MdbxEnv>,
}

impl EeNodeDbMdbx {
    /// Wraps an already-open environment whose tables include the node tables.
    pub fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }

    /// Opens a standalone environment at `path` with just the node tables.
    pub fn open(path: &Path, config: &MdbxConfig) -> DbResult<Self> {
        let env = MdbxEnv::open(path, config, &node_tables())?;
        Ok(Self::new(Arc::new(env)))
    }
}

/// Decodes a stored block record into its domain form.
fn decode_block(db_block: DBExecBlockRecord) -> DbResult<ExecBlockRecord> {
    db_block
        .try_into()
        .map_err(|err| DbError::Other(format!("Failed to decode block: {err:?}")))
}

impl EeNodeDb for EeNodeDbMdbx {
    fn store_ee_account_state(
        &self,
        ol_epoch: EpochCommitment,
        ee_account_state: EeAccountState,
    ) -> DbResult<()> {
        if ol_epoch.is_null() {
            return Err(DbError::NullOLBlock);
        }

        let epoch = ol_epoch.epoch();
        let blockid: DBOLBlockId = (*ol_epoch.last_blkid()).into();
        let account_state =
            DBAccountStateAtEpoch::from_parts(epoch, ol_epoch.last_slot(), ee_account_state.into());

        self.env.update(|w| {
            // Single writer: reading `last` inside the write txn is consistent.
            if let Some((last_epoch, _)) = w.last::<OLBlockAtEpochSchema>()? {
                if epoch != last_epoch + 1 {
                    return Err(DbError::skipped_ol_slot(last_epoch.into(), epoch.into()));
                }
            }
            if w.get::<OLBlockAtEpochSchema>(&epoch)?.is_some() {
                return Err(DbError::TxnFilledOLSlot(epoch.into()));
            }

            w.put::<OLBlockAtEpochSchema>(&epoch, &blockid)?;
            w.put::<AccountStateAtOLEpochSchema>(&blockid, &account_state)?;
            Ok(())
        })
    }

    fn rollback_ee_account_state(&self, to_epoch: u32) -> DbResult<()> {
        self.env.update(|w| {
            let Some((max_epoch, _)) = w.last::<OLBlockAtEpochSchema>()? else {
                warn!("called rollback_ee_account_state on empty db");
                return Ok(());
            };
            let Some((min_epoch, _)) = w.first::<OLBlockAtEpochSchema>()? else {
                error!("database should not be empty!!!");
                return Ok(());
            };

            let min_epoch = min_epoch.max(to_epoch + 1);
            for epoch in (min_epoch..=max_epoch).rev() {
                let Some(blockid) = w.get::<OLBlockAtEpochSchema>(&epoch)? else {
                    warn!(%epoch, "expected block to exist in db");
                    continue;
                };
                w.delete::<OLBlockAtEpochSchema>(&epoch)?;
                w.delete::<AccountStateAtOLEpochSchema>(&blockid)?;
            }
            Ok(())
        })
    }

    fn get_ol_blockid(&self, epoch: u32) -> DbResult<Option<OLBlockId>> {
        self.env
            .view(|r| Ok(r.get::<OLBlockAtEpochSchema>(&epoch)?.map(Into::into)))
    }

    fn ee_account_state(&self, block_id: OLBlockId) -> DbResult<Option<EeAccountStateAtEpoch>> {
        let block_id: DBOLBlockId = block_id.into();
        self.env.view(|r| {
            let Some(account_state) = r.get::<AccountStateAtOLEpochSchema>(&block_id)? else {
                return Ok(None);
            };
            let (epoch, slot, account_state) = account_state.into_parts();
            let ol_epoch = EpochCommitment::new(epoch, slot, block_id.into());
            Ok(Some(EeAccountStateAtEpoch::new(
                ol_epoch,
                account_state.into(),
            )))
        })
    }

    fn best_ee_account_state(&self) -> DbResult<Option<EeAccountStateAtEpoch>> {
        self.env.view(|r| {
            let Some((_, block_id)) = r.last::<OLBlockAtEpochSchema>()? else {
                return Ok(None);
            };
            let Some(account_state) = r.get::<AccountStateAtOLEpochSchema>(&block_id)? else {
                return Err(DbError::MissingAccountState(block_id.into()));
            };
            let (epoch, slot, account_state) = account_state.into_parts();
            let ol_epoch = EpochCommitment::new(epoch, slot, block_id.into());
            Ok(Some(EeAccountStateAtEpoch::new(
                ol_epoch,
                account_state.into(),
            )))
        })
    }

    fn save_exec_block(&self, block: ExecBlockRecord, payload: Vec<u8>) -> DbResult<()> {
        let hash = block.blockhash();
        let height = block.blocknum();
        let db_block = block.into();

        self.env.update(|w| {
            if w.get::<ExecBlockSchema>(&hash)?.is_none() {
                w.put::<ExecBlockSchema>(&hash, &db_block)?;
                w.put::<ExecBlockPayloadSchema>(&hash, &payload)?;
            }

            let mut hashes_at_height = w.get::<ExecBlocksAtHeightSchema>(&height)?.unwrap_or_default();
            if hashes_at_height.contains(&hash) {
                warn!(blockhash = ?hash, "Inconsistent DB state; blockhash present in height index without corresponding exec block");
            } else {
                hashes_at_height.push(hash);
                w.put::<ExecBlocksAtHeightSchema>(&height, &hashes_at_height)?;
            }
            Ok(())
        })
    }

    fn init_finalized_chain(&self, hash: Hash) -> DbResult<()> {
        self.env.update(|w| {
            if let Some(existing_genesis_hash) = w.get::<ExecBlockFinalizedSchema>(&0)? {
                if existing_genesis_hash == hash {
                    return Ok(());
                }
                return Err(DbError::FinalizedExecChainGenesisBlockMismatch);
            }

            let db_block = w
                .get::<ExecBlockSchema>(&hash)?
                .ok_or(DbError::MissingExecBlock(hash))?;
            let block = decode_block(db_block)?;
            let height = block.blocknum();
            if height != 0 {
                return Err(DbError::Other(format!(
                    "init_finalized_chain called with non-genesis block at height {height}"
                )));
            }
            w.put::<ExecBlockFinalizedSchema>(&height, &hash)?;
            Ok(())
        })
    }

    fn extend_finalized_chain(&self, new_tip: Hash) -> DbResult<()> {
        // Single writer: the whole walk + insert runs atomically in one txn,
        // so no tip-shift retry or in-txn re-check is needed.
        self.env.update(|w| {
            let (last_finalized_height, last_finalized_blockhash) = w
                .last::<ExecBlockFinalizedSchema>()?
                .ok_or(DbError::FinalizedExecChainEmpty)?;

            if new_tip == last_finalized_blockhash {
                return Ok(());
            }

            let tip_block = decode_block(
                w.get::<ExecBlockSchema>(&new_tip)?
                    .ok_or(DbError::MissingExecBlock(new_tip))?,
            )?;

            if tip_block.blocknum() <= last_finalized_height {
                // Another finalization may already cover this tip.
                let already_finalized = matches!(
                    w.get::<ExecBlockFinalizedSchema>(&tip_block.blocknum())?,
                    Some(finalized) if finalized == new_tip
                );
                if already_finalized {
                    trace!(
                        ?new_tip,
                        tip_blocknum = tip_block.blocknum(),
                        last_finalized_height,
                        "new_tip already finalized; no-op"
                    );
                    return Ok(());
                }
                return Err(DbError::ExecBlockDoesNotExtendChain(new_tip));
            }

            // Walk parent links from `new_tip` back to the current finalized tip.
            let max_steps = tip_block.blocknum() - last_finalized_height;
            let mut pending_entries_rev = Vec::new();
            let mut current_hash = new_tip;
            let mut current_block = tip_block;
            let mut found_child_of_tip = false;

            for _ in 0..max_steps {
                if current_block.blocknum() <= last_finalized_height {
                    return Err(DbError::FinalizedWalkNotDescending {
                        new_tip,
                        finalized_height: last_finalized_height,
                    });
                }
                pending_entries_rev.push((current_block.blocknum(), current_hash));

                if current_block.parent_blockhash() == last_finalized_blockhash {
                    found_child_of_tip = true;
                    break;
                }

                current_hash = current_block.parent_blockhash();
                current_block = decode_block(
                    w.get::<ExecBlockSchema>(&current_hash)?
                        .ok_or(DbError::MissingExecBlock(current_hash))?,
                )?;
            }
            if !found_child_of_tip {
                return Err(DbError::FinalizedWalkStepBudgetExceeded {
                    new_tip,
                    finalized_height: last_finalized_height,
                    max_steps,
                });
            }

            pending_entries_rev.reverse();

            // Defense in depth: verify contiguous heights before inserting.
            for (offset, (height, _)) in pending_entries_rev.iter().enumerate() {
                let expected_height = last_finalized_height + offset as u64 + 1;
                if *height != expected_height {
                    return Err(DbError::FinalizedWalkNotDescending {
                        new_tip,
                        finalized_height: last_finalized_height,
                    });
                }
            }

            for (height, hash) in &pending_entries_rev {
                w.put::<ExecBlockFinalizedSchema>(height, hash)?;
            }
            Ok(())
        })
    }

    fn revert_finalized_chain(&self, to_height: u64) -> DbResult<()> {
        self.env.update(|w| {
            let Some((current_height, _)) = w.last::<ExecBlockFinalizedSchema>()? else {
                return Err(DbError::FinalizedExecChainEmpty);
            };
            if current_height <= to_height {
                return Ok(());
            }
            for height in (to_height + 1)..=current_height {
                w.delete::<ExecBlockFinalizedSchema>(&height)?;
            }
            Ok(())
        })
    }

    fn prune_block_data(&self, to_height: u64) -> DbResult<()> {
        self.env.update(|w| {
            let mut hashes_to_prune = Vec::new();
            let mut heights_to_remove = Vec::new();
            w.for_each::<ExecBlocksAtHeightSchema>(|height, hashes| {
                if height < to_height {
                    hashes_to_prune.extend(hashes);
                    heights_to_remove.push(height);
                }
                Ok(())
            })?;

            for hash in &hashes_to_prune {
                w.delete::<ExecBlockSchema>(hash)?;
                w.delete::<ExecBlockPayloadSchema>(hash)?;
            }
            for height in &heights_to_remove {
                w.delete::<ExecBlocksAtHeightSchema>(height)?;
            }
            Ok(())
        })
    }

    fn best_finalized_block(&self) -> DbResult<Option<ExecBlockRecord>> {
        self.env.view(|r| {
            let Some((_, best_blockhash)) = r.last::<ExecBlockFinalizedSchema>()? else {
                return Ok(None);
            };
            match r.get::<ExecBlockSchema>(&best_blockhash)? {
                Some(db_block) => Ok(Some(decode_block(db_block)?)),
                None => Ok(None),
            }
        })
    }

    fn get_finalized_block_at_height(&self, height: u64) -> DbResult<Option<ExecBlockRecord>> {
        self.env.view(|r| {
            let Some(blockhash) = r.get::<ExecBlockFinalizedSchema>(&height)? else {
                return Ok(None);
            };
            match r.get::<ExecBlockSchema>(&blockhash)? {
                Some(db_block) => Ok(Some(decode_block(db_block)?)),
                None => Ok(None),
            }
        })
    }

    fn get_finalized_height(&self, hash: Hash) -> DbResult<Option<u64>> {
        self.env.view(|r| {
            let Some(height) = r.get::<ExecBlockSchema>(&hash)?.map(|block| block.blocknum) else {
                return Ok(None);
            };
            let Some(finalized_blockhash) = r.get::<ExecBlockFinalizedSchema>(&height)? else {
                return Ok(None);
            };
            if finalized_blockhash != hash {
                return Ok(None);
            }
            Ok(Some(height))
        })
    }

    fn get_unfinalized_blocks(&self) -> DbResult<Vec<Hash>> {
        self.env.view(|r| {
            let (finalized_height, _) = r
                .last::<ExecBlockFinalizedSchema>()?
                .ok_or(DbError::FinalizedExecChainEmpty)?;

            let Some((last_unfinalized_height, _)) = r.last::<ExecBlocksAtHeightSchema>()? else {
                warn!("exec_blocks_by_height index is empty");
                return Ok(Vec::new());
            };

            let mut unfinalized_hashes = Vec::new();
            for height in (finalized_height + 1)..=last_unfinalized_height {
                if let Some(mut blockhashes) = r.get::<ExecBlocksAtHeightSchema>(&height)? {
                    unfinalized_hashes.append(&mut blockhashes);
                }
            }
            Ok(unfinalized_hashes)
        })
    }

    fn get_exec_block(&self, hash: Hash) -> DbResult<Option<ExecBlockRecord>> {
        self.env.view(|r| match r.get::<ExecBlockSchema>(&hash)? {
            Some(db_block) => Ok(Some(decode_block(db_block)?)),
            None => Ok(None),
        })
    }

    fn get_block_payload(&self, hash: Hash) -> DbResult<Option<Vec<u8>>> {
        self.env
            .view(|r| Ok(r.get::<ExecBlockPayloadSchema>(&hash)?))
    }

    fn delete_exec_block(&self, hash: Hash) -> DbResult<()> {
        self.env.update(|w| {
            let Some(height) = w.get::<ExecBlockSchema>(&hash)?.map(|block| block.blocknum) else {
                return Ok(());
            };

            if let Some(finalized_hash) = w.get::<ExecBlockFinalizedSchema>(&height)? {
                if finalized_hash == hash {
                    return Err(DbError::CannotDeleteFinalizedBlock(hash));
                }
            }

            w.delete::<ExecBlockSchema>(&hash)?;
            w.delete::<ExecBlockPayloadSchema>(&hash)?;

            if let Some(mut hashes_at_height) = w.get::<ExecBlocksAtHeightSchema>(&height)? {
                hashes_at_height.retain(|&h| h != hash);
                if hashes_at_height.is_empty() {
                    w.delete::<ExecBlocksAtHeightSchema>(&height)?;
                } else {
                    w.put::<ExecBlocksAtHeightSchema>(&height, &hashes_at_height)?;
                }
            }
            Ok(())
        })
    }

    fn save_genesis_batch(&self, batch: Batch) -> DbResult<()> {
        let idx = batch.idx();
        let batch_id: DBBatchId = batch.id().into();
        let db_batch = DBBatchWithStatus::new(batch, BatchStatus::Genesis);

        self.env.update(|w| {
            if w.first::<BatchByIdxSchema>()?.is_some() {
                return Ok(());
            }
            w.put::<BatchByIdxSchema>(&idx, &db_batch)?;
            w.put::<BatchIdToIdxSchema>(&batch_id, &idx)?;
            Ok(())
        })
    }

    fn save_next_batch(&self, batch: Batch) -> DbResult<()> {
        self.env.update(|w| {
            let Some((_, last_db_batch)) = w.last::<BatchByIdxSchema>()? else {
                return Err(DbError::Other(
                    "cannot save next batch: no previous batch exists".into(),
                ));
            };
            let (last_batch, _) = last_db_batch
                .into_parts()
                .map_err(|e| DbError::BatchDeserialize(e.to_string()))?;

            if batch.prev_block() != last_batch.last_block() {
                return Err(DbError::Other(format!(
                    "batch does not extend previous batch: expected prev_block {:?}, got {:?}",
                    last_batch.last_block(),
                    batch.prev_block()
                )));
            }
            if batch.idx() != last_batch.idx() + 1 {
                return Err(DbError::Other(format!(
                    "batch idx is not sequential: expected {}, got {}",
                    last_batch.idx() + 1,
                    batch.idx()
                )));
            }

            let idx = batch.idx();
            let batch_id: DBBatchId = batch.id().into();
            let db_batch = DBBatchWithStatus::new(batch, BatchStatus::Sealed);
            w.put::<BatchByIdxSchema>(&idx, &db_batch)?;
            w.put::<BatchIdToIdxSchema>(&batch_id, &idx)?;
            Ok(())
        })
    }

    fn update_batch_status(&self, batch_id: BatchId, status: BatchStatus) -> DbResult<()> {
        let db_batch_id: DBBatchId = batch_id.into();
        self.env.update(|w| {
            let Some(idx) = w.get::<BatchIdToIdxSchema>(&db_batch_id)? else {
                return Err(DbError::BatchNotFound(batch_id));
            };
            let Some(current) = w.get::<BatchByIdxSchema>(&idx)? else {
                return Err(DbError::BatchNotFound(batch_id));
            };
            let (batch, _old_status) = current
                .into_parts()
                .map_err(|e| DbError::BatchDeserialize(e.to_string()))?;
            if batch.id() != batch_id {
                return Err(DbError::BatchNotFound(batch_id));
            }
            let updated = DBBatchWithStatus::new(batch, status.clone());
            w.put::<BatchByIdxSchema>(&idx, &updated)?;
            Ok(())
        })
    }

    fn revert_batches(&self, to_idx: u64) -> DbResult<()> {
        self.env.update(|w| {
            let Some((max_idx, _)) = w.last::<BatchByIdxSchema>()? else {
                return Ok(());
            };
            if max_idx <= to_idx {
                return Ok(());
            }
            for idx in (to_idx + 1)..=max_idx {
                let Some(db_batch) = w.get::<BatchByIdxSchema>(&idx)? else {
                    continue;
                };
                if let Ok((batch, _)) = db_batch.into_parts() {
                    let batch_id = DBBatchId::from(batch.id());
                    w.delete::<BatchByIdxSchema>(&idx)?;
                    w.delete::<BatchIdToIdxSchema>(&batch_id)?;
                    w.delete::<BatchChunksSchema>(&batch_id)?;
                }
            }
            Ok(())
        })
    }

    fn get_batch_by_id(&self, batch_id: BatchId) -> DbResult<Option<(Batch, BatchStatus)>> {
        let db_batch_id: DBBatchId = batch_id.into();
        self.env.view(|r| {
            let Some(idx) = r.get::<BatchIdToIdxSchema>(&db_batch_id)? else {
                return Ok(None);
            };
            let Some(db_batch) = r.get::<BatchByIdxSchema>(&idx)? else {
                return Ok(None);
            };
            let (batch, status) = db_batch
                .into_parts()
                .map_err(|e| DbError::BatchDeserialize(e.to_string()))?;
            Ok(Some((batch, status)))
        })
    }

    fn get_batch_by_idx(&self, idx: u64) -> DbResult<Option<(Batch, BatchStatus)>> {
        self.env.view(|r| {
            let Some(db_batch) = r.get::<BatchByIdxSchema>(&idx)? else {
                return Ok(None);
            };
            let (batch, status) = db_batch
                .into_parts()
                .map_err(|e| DbError::BatchDeserialize(e.to_string()))?;
            Ok(Some((batch, status)))
        })
    }

    fn get_latest_batch(&self) -> DbResult<Option<(Batch, BatchStatus)>> {
        self.env.view(|r| {
            let Some((_, db_batch)) = r.last::<BatchByIdxSchema>()? else {
                return Ok(None);
            };
            let (batch, status) = db_batch
                .into_parts()
                .map_err(|e| DbError::BatchDeserialize(e.to_string()))?;
            Ok(Some((batch, status)))
        })
    }

    fn save_next_chunk(&self, chunk: Chunk) -> DbResult<()> {
        let idx = chunk.idx();
        let chunk_id: DBChunkId = chunk.id().into();
        let db_chunk = DBChunkWithStatus::new(chunk, ChunkStatus::ProvingNotStarted);

        self.env.update(|w| {
            w.put::<ChunkByIdxSchema>(&idx, &db_chunk)?;
            w.put::<ChunkIdToIdxSchema>(&chunk_id, &idx)?;
            Ok(())
        })
    }

    fn update_chunk_status(&self, chunk_id: ChunkId, status: ChunkStatus) -> DbResult<()> {
        let db_chunk_id: DBChunkId = chunk_id.into();
        self.env.update(|w| {
            let Some(idx) = w.get::<ChunkIdToIdxSchema>(&db_chunk_id)? else {
                return Err(DbError::ChunkNotFound(chunk_id));
            };
            let Some(current) = w.get::<ChunkByIdxSchema>(&idx)? else {
                return Err(DbError::ChunkNotFound(chunk_id));
            };
            let (chunk, _old_status) = current.into_parts();
            if chunk.id() != chunk_id {
                return Err(DbError::ChunkNotFound(chunk_id));
            }
            let updated = DBChunkWithStatus::new(chunk, status.clone());
            w.put::<ChunkByIdxSchema>(&idx, &updated)?;
            Ok(())
        })
    }

    fn revert_chunks_from(&self, from_idx: u64) -> DbResult<()> {
        self.env.update(|w| {
            let Some((max_idx, _)) = w.last::<ChunkByIdxSchema>()? else {
                return Ok(());
            };
            if max_idx < from_idx {
                return Ok(());
            }
            for idx in from_idx..=max_idx {
                let Some(db_chunk) = w.get::<ChunkByIdxSchema>(&idx)? else {
                    continue;
                };
                let (chunk, _) = db_chunk.into_parts();
                let chunk_id = DBChunkId::from(chunk.id());
                w.delete::<ChunkByIdxSchema>(&idx)?;
                w.delete::<ChunkIdToIdxSchema>(&chunk_id)?;
            }
            Ok(())
        })
    }

    fn get_chunk_by_id(&self, chunk_id: ChunkId) -> DbResult<Option<(Chunk, ChunkStatus)>> {
        let db_chunk_id: DBChunkId = chunk_id.into();
        self.env.view(|r| {
            let Some(idx) = r.get::<ChunkIdToIdxSchema>(&db_chunk_id)? else {
                return Ok(None);
            };
            let Some(db_chunk) = r.get::<ChunkByIdxSchema>(&idx)? else {
                return Ok(None);
            };
            Ok(Some(db_chunk.into_parts()))
        })
    }

    fn get_chunk_by_idx(&self, idx: u64) -> DbResult<Option<(Chunk, ChunkStatus)>> {
        self.env.view(|r| match r.get::<ChunkByIdxSchema>(&idx)? {
            Some(db_chunk) => Ok(Some(db_chunk.into_parts())),
            None => Ok(None),
        })
    }

    fn get_latest_chunk(&self) -> DbResult<Option<(Chunk, ChunkStatus)>> {
        self.env.view(|r| match r.last::<ChunkByIdxSchema>()? {
            Some((_, db_chunk)) => Ok(Some(db_chunk.into_parts())),
            None => Ok(None),
        })
    }

    fn set_batch_chunks(&self, batch_id: BatchId, chunks: Vec<ChunkId>) -> DbResult<()> {
        let db_batch_id: DBBatchId = batch_id.into();
        let db_chunks: Vec<DBChunkId> = chunks.into_iter().map(Into::into).collect();
        self.env.update(|w| {
            w.put::<BatchChunksSchema>(&db_batch_id, &db_chunks)?;
            Ok(())
        })
    }

    fn get_batch_chunks(&self, batch_id: BatchId) -> DbResult<Option<Vec<ChunkId>>> {
        let db_batch_id: DBBatchId = batch_id.into();
        self.env.view(|r| {
            let Some(db_chunks) = r.get::<BatchChunksSchema>(&db_batch_id)? else {
                return Ok(None);
            };
            Ok(Some(db_chunks.into_iter().map(Into::into).collect()))
        })
    }

    fn put_block_witness(&self, block_id: Hash, witness: Vec<u8>) -> DbResult<()> {
        self.env.update(|w| {
            w.put::<BlockWitnessSchema>(&block_id, &witness)?;
            Ok(())
        })
    }

    fn get_block_witness(&self, block_id: Hash) -> DbResult<Option<Vec<u8>>> {
        self.env
            .view(|r| Ok(r.get::<BlockWitnessSchema>(&block_id)?))
    }

    fn del_block_witness(&self, block_id: Hash) -> DbResult<()> {
        self.env.update(|w| {
            w.delete::<BlockWitnessSchema>(&block_id)?;
            Ok(())
        })
    }

    fn put_block_accessed_state(
        &self,
        block_id: Hash,
        record: AccessedStateRecord,
    ) -> DbResult<()> {
        self.env.update(|w| {
            w.put::<BlockAccessedStateSchema>(&block_id, &record)?;
            Ok(())
        })
    }

    fn get_block_accessed_state(&self, block_id: Hash) -> DbResult<Option<AccessedStateRecord>> {
        self.env
            .view(|r| Ok(r.get::<BlockAccessedStateSchema>(&block_id)?))
    }

    fn del_block_accessed_state(&self, block_id: Hash) -> DbResult<()> {
        self.env.update(|w| {
            w.delete::<BlockAccessedStateSchema>(&block_id)?;
            Ok(())
        })
    }

    fn put_bytecode(&self, code_hash: Hash, code: Vec<u8>) -> DbResult<()> {
        self.env.update(|w| {
            w.put::<BytecodeSchema>(&code_hash, &code)?;
            Ok(())
        })
    }

    fn get_bytecode(&self, code_hash: Hash) -> DbResult<Option<Vec<u8>>> {
        self.env.view(|r| Ok(r.get::<BytecodeSchema>(&code_hash)?))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    };

    use alpen_db_store_mdbx::MdbxConfig;
    use alpen_ee_common::{
        batch_storage_tests, chunk_storage_tests, exec_block_storage_test_fns::create_exec_block,
        exec_block_storage_tests, storage_tests,
    };
    use tokio::runtime::{Handle, Runtime};

    use super::*;
    use crate::storage::EeNodeStorage;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Opens a fresh MDBX-backed node db under a unique temp path.
    fn temp_db() -> EeNodeDbMdbx {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("ee-mdbx-node-test-{}-{n}", process::id()));
        EeNodeDbMdbx::open(&path, &MdbxConfig::small()).unwrap()
    }

    /// A process-wide tokio runtime handle for the storage tests.
    fn test_runtime_handle() -> Handle {
        use std::sync::OnceLock;
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().expect("test: build runtime"))
            .handle()
            .clone()
    }

    fn setup_storage() -> EeNodeStorage {
        EeNodeStorage::new(test_runtime_handle(), Arc::new(temp_db()))
    }

    storage_tests!(setup_storage());
    exec_block_storage_tests!(setup_storage());
    batch_storage_tests!(setup_storage());
    chunk_storage_tests!(setup_storage());

    fn hash_from_u8(value: u8) -> Hash {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[31] = value;
        Hash::from(bytes)
    }

    fn save_block(db: &EeNodeDbMdbx, block: ExecBlockRecord) {
        db.save_exec_block(block, vec![]).unwrap();
    }

    #[test]
    fn extend_finalized_chain_ok_if_tip_already_finalized() {
        let db = temp_db();
        let h0 = hash_from_u8(0);
        let h1 = hash_from_u8(1);
        let h2 = hash_from_u8(2);

        save_block(&db, create_exec_block(0, Hash::default(), h0, 0));
        save_block(&db, create_exec_block(1, h0, h1, 1));
        save_block(&db, create_exec_block(2, h1, h2, 2));

        db.init_finalized_chain(h0).unwrap();
        db.extend_finalized_chain(h2).unwrap();

        // Simulates "caller behind" after finalization already advanced past h1.
        db.extend_finalized_chain(h1).unwrap();

        assert_eq!(db.get_finalized_height(h1).unwrap(), Some(1));
        assert_eq!(db.get_finalized_height(h2).unwrap(), Some(2));
        let best = db.best_finalized_block().unwrap().unwrap();
        assert_eq!(best.blockhash(), h2);
        assert_eq!(best.blocknum(), 2);
    }

    #[test]
    fn extend_finalized_chain_cycle_errors_with_step_budget_exceeded() {
        let db = temp_db();
        let h0 = hash_from_u8(0);
        let h2 = hash_from_u8(2);
        let h3 = hash_from_u8(3);

        save_block(&db, create_exec_block(0, Hash::default(), h0, 0));
        db.init_finalized_chain(h0).unwrap();

        // Corrupt graph above finalized tip: h3 -> h2 and h2 -> h3 (cycle).
        save_block(&db, create_exec_block(2, h3, h2, 2));
        save_block(&db, create_exec_block(3, h2, h3, 3));

        let err = db.extend_finalized_chain(h3).unwrap_err();
        assert!(matches!(
            err,
            DbError::FinalizedWalkStepBudgetExceeded {
                new_tip,
                finalized_height: 0,
                max_steps: 3,
            } if new_tip == h3
        ));
    }
}
