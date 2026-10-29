//! [`DaBlobSource`] implementation backed by Reth state diffs.

use std::{collections::BTreeSet, fmt, mem, sync::Arc};

use alloy_primitives::{Bytes, B256, KECCAK256_EMPTY};
use alpen_common::{
    BatchId, BatchStorage, DaContext, EvmCodeProvider, HeaderSummaryProvider, StateDiffProvider,
};
use alpen_da_types::DaBlob;
use alpen_reth_statediff::{AccountChange, BatchBuilder, BatchStateDiff};
use async_trait::async_trait;
use tracing::*;

use crate::DaBlobSource;

/// [`DaBlobSource`] that builds encoded DA blobs from Reth state diffs.
///
/// For each batch, it:
/// 1. Retrieves the block range from [`BatchStorage`].
/// 2. Fetches per-block [`BlockStateChanges`](alpen_reth_statediff::BlockStateChanges) from the
///    [`StateDiffProvider`].
/// 3. Aggregates them into a [`BatchStateDiff`](alpen_reth_statediff::BatchStateDiff) via
///    [`BatchBuilder`], keeping the bytecodes its accounts reference minus those an account already
///    holds in the batch's pre-state.
/// 4. Reads the last block's header to build
///    [`EvmHeaderSummary`](alpen_da_types::EvmHeaderSummary).
/// 5. Returns the assembled [`DaBlob`].
pub struct StateDiffBlobProvider<S, D, H, C> {
    batch_storage: Arc<S>,
    state_diff_provider: Arc<D>,
    header_summary: Arc<H>,
    evm_code: Arc<C>,
    da_ctx: Arc<dyn DaContext + Send + Sync>,
}

impl<S, D, H, C> fmt::Debug for StateDiffBlobProvider<S, D, H, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateDiffBlobProvider")
            .finish_non_exhaustive()
    }
}

impl<S, D, H, C> StateDiffBlobProvider<S, D, H, C> {
    pub fn new(
        batch_storage: Arc<S>,
        state_diff_provider: Arc<D>,
        header_summary: Arc<H>,
        evm_code: Arc<C>,
        da_ctx: Arc<dyn DaContext + Send + Sync>,
    ) -> Self {
        Self {
            batch_storage,
            state_diff_provider,
            header_summary,
            evm_code,
            da_ctx,
        }
    }
}

impl<S, D, H, C: EvmCodeProvider> StateDiffBlobProvider<S, D, H, C> {
    /// Returns `true` if a known holder of the bytecode still holds it in the
    /// state after `pre_state_block`, so the blob may leave it out.
    ///
    /// On errors the bytecode is conservatively kept — duplicates are safe,
    /// missing data is not.
    fn held_in_pre_state(&self, pre_state_block: B256, code_hash: &B256) -> bool {
        let holder = match self.da_ctx.code_hash_holder(code_hash) {
            Ok(Some(holder)) => holder,
            Ok(None) => return false,
            Err(e) => {
                warn!(?code_hash, error = %e, "failed to look up code holder, keeping bytecode");
                return false;
            }
        };
        match self.evm_code.code_hash_at(pre_state_block, holder) {
            Ok(held) => held == Some(*code_hash),
            Err(e) => {
                warn!(?code_hash, %holder, error = %e, "failed to read holder code, keeping bytecode");
                false
            }
        }
    }
}

/// Rebuilds `diff`'s bytecodes so the blob carries exactly the code its
/// accounts reference and no pre-state account already holds. Returns how many
/// referenced bytecodes were left out.
///
/// The account proof guest accepts a referenced code hash only if the blob
/// carries its bytecode or an account holds it in the pre-state, so the two
/// sides must apply this same rule. Bytecodes no account references, such as
/// those of contracts created and destroyed in one transaction, are dropped.
fn select_published_bytecodes(
    diff: &mut BatchStateDiff,
    held_in_pre_state: impl Fn(&B256) -> bool,
    fetch_bytecode: impl Fn(B256) -> eyre::Result<Option<Bytes>>,
) -> eyre::Result<usize> {
    let mut deployed = mem::take(&mut diff.deployed_bytecodes);
    let mut deduped = 0;
    for code_hash in referenced_code_hashes(diff) {
        if held_in_pre_state(&code_hash) {
            deduped += 1;
            continue;
        }
        let bytecode = match deployed.remove(&code_hash) {
            Some(bytecode) => bytecode,
            None => fetch_bytecode(code_hash)?.ok_or_else(|| {
                eyre::eyre!("no bytecode for code hash {code_hash} referenced by the batch")
            })?,
        };
        diff.deployed_bytecodes.insert(code_hash, bytecode);
    }
    Ok(deduped)
}

/// Returns the non-empty code hashes the diff's accounts take on.
fn referenced_code_hashes(diff: &BatchStateDiff) -> BTreeSet<B256> {
    diff.accounts
        .values()
        .filter_map(|change| match change {
            AccountChange::Created(diff) | AccountChange::Updated(diff) => {
                diff.code_hash.new_value().map(|hash| hash.0)
            }
            AccountChange::Deleted => None,
        })
        .filter(|code_hash| *code_hash != KECCAK256_EMPTY)
        .collect()
}

#[async_trait]
impl<S, D, H, C> DaBlobSource for StateDiffBlobProvider<S, D, H, C>
where
    S: BatchStorage,
    D: StateDiffProvider,
    H: HeaderSummaryProvider,
    C: EvmCodeProvider,
{
    async fn get_blob(&self, batch_id: BatchId) -> eyre::Result<DaBlob> {
        // 1. Look up the batch to get its block range.
        let (batch, _status) = self
            .batch_storage
            .get_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| eyre::eyre!("batch {batch_id:?} not found in storage"))?;

        // 2. Aggregate per-block diffs via BatchBuilder.
        let mut builder = BatchBuilder::new();
        let mut block_count = 0u64;

        for block_hash in batch.blocks_iter() {
            // Convert Hash (Buf32) -> B256 for the StateDiffProvider interface.
            let b256 = B256::from(block_hash.0);

            match self.state_diff_provider.get_state_diff_by_hash(b256) {
                Ok(Some(block_diff)) => {
                    builder.apply_block(&block_diff);
                    block_count += 1;
                }
                Ok(None) => {
                    warn!(?block_hash, "no state diff for block, skipping");
                }
                Err(e) => {
                    warn!(?block_hash, error = %e, "failed to fetch state diff for block");
                    return Err(eyre::eyre!(
                        "failed to fetch state diff for block {block_hash:?}: {e}"
                    ));
                }
            }
        }

        let expected_count = batch.blocks_iter().count() as u64;
        eyre::ensure!(
            block_count == expected_count,
            "state diff count mismatch for batch {batch_id:?}: got {block_count}, expected {expected_count}"
        );

        // 3. Build the aggregate diff and leave out the bytecodes an account already holds in the
        //    batch's pre-state.
        let mut state_diff = builder.build();
        let pre_state_block = B256::from(batch.prev_block().0);
        let deduped = select_published_bytecodes(
            &mut state_diff,
            |code_hash| self.held_in_pre_state(pre_state_block, code_hash),
            |code_hash| self.evm_code.bytecode(code_hash),
        )?;

        info!(
            ?batch_id,
            block_count,
            is_empty = state_diff.is_empty(),
            deduped,
            "built DA blob for batch"
        );

        // 4. Read the last block's header for chain-reconstruction metadata.
        let evm_header = self
            .header_summary
            .header_summary(batch.last_blocknum(), batch.spec_version())?;

        let update_seq_no = batch
            .update_seq_no()
            .ok_or_else(|| eyre::eyre!("cannot build DA blob for genesis batch"))?;

        // 5. Construct the DaBlob with metadata.
        Ok(DaBlob {
            spec_version: batch.spec_version(),
            update_seq_no,
            evm_header,
            state_diff,
        })
    }

    async fn are_state_diffs_ready(&self, batch_id: BatchId) -> bool {
        // Look up the batch to get its block range.
        let Ok(Some((batch, _status))) = self.batch_storage.get_batch_by_id(batch_id).await else {
            warn!(?batch_id, "batch not found or lookup failed");
            return false;
        };

        // Check if all blocks have state diffs available.
        for block_hash in batch.blocks_iter() {
            let b256 = B256::from(block_hash.0);
            match self.state_diff_provider.get_state_diff_by_hash(b256) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    warn!(?block_hash, "state diff not available for block");
                    return false;
                }
                Err(e) => {
                    warn!(?block_hash, error = %e, "failed to check state diff for block");
                    return false;
                }
            }
        }

        true
    }

    async fn mark_batch_published(&self, batch_id: BatchId) -> eyre::Result<()> {
        let (batch, _status) = self
            .batch_storage
            .get_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| eyre::eyre!("batch {batch_id:?} not found in storage"))?;

        let block_hashes: Vec<B256> = batch.blocks_iter().map(|h| B256::from(h.0)).collect();

        self.da_ctx
            .update_da_filter(&block_hashes)
            .map_err(|e| eyre::eyre!("failed to update DA filter for batch {batch_id:?}: {e}"))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{keccak256, Address, U256};
    use alpen_reth_statediff::AccountDiff;

    use super::*;

    fn created_with_code(diff: &mut BatchStateDiff, address: u8, code_hash: B256) {
        diff.accounts.insert(
            Address::from([address; 20]),
            AccountChange::Created(AccountDiff::new_created(U256::ZERO, 1, code_hash)),
        );
    }

    fn no_fetch(code_hash: B256) -> eyre::Result<Option<Bytes>> {
        panic!("unexpected bytecode fetch for {code_hash}")
    }

    #[test]
    fn leaves_out_bytecode_a_pre_state_account_holds() {
        let held = Bytes::from_static(&[0x60, 0x01]);
        let fresh = Bytes::from_static(&[0x60, 0x02]);
        let mut diff = BatchStateDiff::new();
        created_with_code(&mut diff, 1, keccak256(&held));
        created_with_code(&mut diff, 2, keccak256(&fresh));
        diff.deployed_bytecodes
            .insert(keccak256(&held), held.clone());
        diff.deployed_bytecodes
            .insert(keccak256(&fresh), fresh.clone());

        let deduped = select_published_bytecodes(
            &mut diff,
            |code_hash| *code_hash == keccak256(&held),
            no_fetch,
        )
        .unwrap();

        assert_eq!(deduped, 1);
        assert_eq!(
            diff.deployed_bytecodes,
            BTreeMap::from([(keccak256(&fresh), fresh)])
        );
    }

    #[test]
    fn fetches_referenced_bytecode_no_holder_covers() {
        let code = Bytes::from_static(&[0x60, 0x03]);
        let code_hash = keccak256(&code);
        let mut diff = BatchStateDiff::new();
        created_with_code(&mut diff, 1, code_hash);

        let deduped = select_published_bytecodes(
            &mut diff,
            |_| false,
            |wanted| Ok((wanted == code_hash).then(|| code.clone())),
        )
        .unwrap();

        assert_eq!(deduped, 0);
        assert_eq!(diff.deployed_bytecodes, BTreeMap::from([(code_hash, code)]));
    }

    #[test]
    fn fails_when_referenced_bytecode_is_nowhere() {
        let mut diff = BatchStateDiff::new();
        created_with_code(&mut diff, 1, B256::from([0xC0; 32]));

        let err = select_published_bytecodes(&mut diff, |_| false, |_| Ok(None))
            .expect_err("a referenced code hash without bytes or holder must fail");

        assert!(err.to_string().contains("no bytecode for code hash"));
    }

    /// Code that no account ends up holding, such as a contract created and
    /// destroyed in one transaction, is dropped, as is the empty code hash.
    #[test]
    fn drops_bytecode_no_account_references() {
        let orphan = Bytes::from_static(&[0x60, 0x04]);
        let mut diff = BatchStateDiff::new();
        created_with_code(&mut diff, 1, KECCAK256_EMPTY);
        diff.deployed_bytecodes.insert(keccak256(&orphan), orphan);

        let deduped = select_published_bytecodes(&mut diff, |_| false, no_fetch).unwrap();

        assert_eq!(deduped, 0);
        assert!(diff.deployed_bytecodes.is_empty());
    }
}
