use std::{path::Path, sync::Arc};

use alloy_primitives::{Address, B256, KECCAK256_EMPTY as KECCAK_EMPTY};
use alpen_common::{DaContext, StateDiffProvider, StateDiffStore};
use alpen_mdbx::{DbError as MdbxError, MdbxConfig, MdbxEnv};
use alpen_reth_statediff::BlockStateChanges;
use strata_db_types::{errors::DbError, DbResult};
use tracing::warn;

use super::schema::{
    witness_tables, BlockHashByNumber, BlockStateChangesSchema, PublishedCodeHashSchema,
};

/// Maps a storage-engine error into the reth state-diff database error type.
fn to_db_error(err: MdbxError) -> DbError {
    DbError::Other(format!("mdbx: {err}"))
}

/// MDBX-backed state-diff store.
#[derive(Debug, Clone)]
pub struct WitnessDbMdbx {
    env: Arc<MdbxEnv>,
}

impl WitnessDbMdbx {
    /// Wraps an already-open environment whose tables include the state-diff
    /// tables.
    pub fn new(env: Arc<MdbxEnv>) -> Self {
        Self { env }
    }

    /// Opens a standalone environment at `path` with just the state-diff tables.
    pub fn open(path: &Path, config: &MdbxConfig) -> DbResult<Self> {
        let env = MdbxEnv::open(path, config, &witness_tables()).map_err(to_db_error)?;
        Ok(Self::new(Arc::new(env)))
    }
}

impl StateDiffProvider for WitnessDbMdbx {
    fn get_state_diff_by_hash(&self, block_hash: B256) -> DbResult<Option<BlockStateChanges>> {
        self.env
            .view(|r| r.get::<BlockStateChangesSchema>(&block_hash))
            .map_err(to_db_error)
    }

    fn get_state_diff_by_number(&self, block_number: u64) -> DbResult<Option<BlockStateChanges>> {
        let block_hash = self
            .env
            .view(|r| r.get::<BlockHashByNumber>(&block_number))
            .map_err(to_db_error)?;
        let Some(block_hash) = block_hash else {
            return Ok(None);
        };
        self.get_state_diff_by_hash(block_hash)
    }
}

impl StateDiffStore for WitnessDbMdbx {
    fn put_state_diff(
        &self,
        block_hash: B256,
        block_number: u64,
        state_diff: &BlockStateChanges,
    ) -> DbResult<()> {
        self.env
            .update(|w| {
                w.put::<BlockHashByNumber>(&block_number, &block_hash)?;
                w.put::<BlockStateChangesSchema>(&block_hash, state_diff)?;
                Ok(())
            })
            .map_err(to_db_error)
    }

    fn del_state_diff(&self, block_hash: B256) -> DbResult<()> {
        self.env
            .update(|w| {
                w.delete::<BlockStateChangesSchema>(&block_hash)?;
                Ok(())
            })
            .map_err(to_db_error)
    }
}

/// Persistent DA filter for the EE, backed by MDBX.
///
/// Tracks which data items (currently contract bytecodes) have already been
/// published to DA so that future batches can omit them.
#[derive(Debug, Clone)]
pub struct DaContextDbMdbx<S> {
    env: Arc<MdbxEnv>,
    state_diff_provider: Arc<S>,
}

impl<S> DaContextDbMdbx<S> {
    /// Wraps an environment whose tables include the published-code-hash table,
    /// reading state diffs through `state_diff_provider`.
    pub fn new(env: Arc<MdbxEnv>, state_diff_provider: Arc<S>) -> Self {
        Self {
            env,
            state_diff_provider,
        }
    }
}

impl<S: StateDiffProvider + 'static> DaContextDbMdbx<S> {
    /// Collects the accounts that took on new code in the given blocks and
    /// records them as holders, so future batches can omit that code.
    ///
    /// Holders come from the account changes rather than the deployed bytecode
    /// map: a contract created and destroyed in one transaction leaves its code
    /// in the map, but no account holds it.
    fn update_bytecode_filter(&self, block_hashes: &[B256]) -> DbResult<()> {
        let mut holders = Vec::new();
        for block_hash in block_hashes {
            match self.state_diff_provider.get_state_diff_by_hash(*block_hash) {
                Ok(Some(diff)) => holders.extend(code_holders(&diff)),
                Ok(None) => {}
                Err(e) => {
                    warn!(%block_hash, error = %e, "failed to fetch state diff for block, skipping");
                }
            }
        }
        if !holders.is_empty() {
            self.mark_code_hashes_published(&holders)?;
        }
        Ok(())
    }
}

/// Returns each account in `diff` that ends the block with code it did not
/// start with, paired with that code's hash.
fn code_holders(diff: &BlockStateChanges) -> impl Iterator<Item = (B256, Address)> + '_ {
    diff.accounts.iter().filter_map(|(address, change)| {
        let code_hash = change.current.as_ref()?.code_hash;
        let original_code_hash = change.original.as_ref().map(|original| original.code_hash);
        (code_hash != KECCAK_EMPTY && original_code_hash != Some(code_hash))
            .then_some((code_hash, *address))
    })
}

impl<S: StateDiffProvider + 'static> DaContext for DaContextDbMdbx<S> {
    fn code_hash_holder(&self, code_hash: &B256) -> DbResult<Option<Address>> {
        let holder = self
            .env
            .view(|r| r.get::<PublishedCodeHashSchema>(code_hash))
            .map_err(to_db_error)?;
        Ok(holder.flatten())
    }

    fn mark_code_hashes_published(&self, holders: &[(B256, Address)]) -> DbResult<()> {
        self.env
            .update(|w| {
                for (code_hash, holder) in holders {
                    if w.get::<PublishedCodeHashSchema>(code_hash)?
                        .flatten()
                        .is_none()
                    {
                        w.put::<PublishedCodeHashSchema>(code_hash, &Some(*holder))?;
                    }
                }
                Ok(())
            })
            .map_err(to_db_error)
    }

    fn update_da_filter(&self, block_hashes: &[B256]) -> DbResult<()> {
        self.update_bytecode_filter(block_hashes)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use alloy_primitives::{address, fixed_bytes, Bytes, FixedBytes, U256};
    use alpen_reth_statediff::{
        AccountSnapshot, BlockAccountChange, BlockStateChanges, BlockStorageDiff,
    };

    use super::*;

    const BLOCK_HASH_ONE: FixedBytes<32> =
        fixed_bytes!("000000000000000000000000f529c70db0800449ebd81fbc6e4221523a989f05");
    const BLOCK_HASH_TWO: FixedBytes<32> =
        fixed_bytes!("0000000000000000000000000a743ba7304efcc9e384ece9be7631e2470e401e");

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_env() -> Arc<MdbxEnv> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("reth-mdbx-statediff-test-{}-{n}", process::id()));
        Arc::new(MdbxEnv::open(&path, &MdbxConfig::small(), &witness_tables()).unwrap())
    }

    fn setup_db() -> WitnessDbMdbx {
        WitnessDbMdbx::new(temp_env())
    }

    fn test_state_diff() -> BlockStateChanges {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045"),
            BlockAccountChange {
                original: None,
                current: Some(AccountSnapshot {
                    balance: U256::from(1000),
                    nonce: 1,
                    code_hash: KECCAK_EMPTY,
                }),
            },
        );

        let mut storage = BTreeMap::new();
        let mut slots = BlockStorageDiff::new();
        slots
            .slots
            .insert(U256::from(1), (U256::ZERO, U256::from(100)));
        storage.insert(
            address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045"),
            slots,
        );

        BlockStateChanges {
            accounts,
            storage,
            deployed_bytecodes: BTreeMap::new(),
        }
    }

    #[test]
    fn set_and_get_state_diff_data() {
        let db = setup_db();
        let diff = test_state_diff();

        db.put_state_diff(BLOCK_HASH_ONE, 1, &diff)
            .expect("failed to put state diff");

        let by_hash = db.get_state_diff_by_hash(BLOCK_HASH_ONE).unwrap().unwrap();
        assert_eq!(by_hash.accounts.len(), diff.accounts.len());
        assert_eq!(by_hash.storage.len(), diff.storage.len());

        let by_number = db.get_state_diff_by_number(1).unwrap().unwrap();
        assert_eq!(by_number.accounts.len(), diff.accounts.len());
    }

    #[test]
    fn del_and_get_state_diff_data() {
        let db = setup_db();
        let diff = test_state_diff();

        assert!(matches!(
            db.get_state_diff_by_hash(BLOCK_HASH_TWO),
            Ok(None)
        ));

        db.put_state_diff(BLOCK_HASH_TWO, 7, &diff).unwrap();
        assert!(matches!(
            db.get_state_diff_by_hash(BLOCK_HASH_TWO),
            Ok(Some(BlockStateChanges { .. }))
        ));

        assert!(matches!(db.del_state_diff(BLOCK_HASH_TWO), Ok(())));
        assert!(matches!(
            db.get_state_diff_by_hash(BLOCK_HASH_TWO),
            Ok(None)
        ));
    }

    fn setup_da_context() -> DaContextDbMdbx<WitnessDbMdbx> {
        let env = temp_env();
        let witness_db = Arc::new(WitnessDbMdbx::new(env.clone()));
        DaContextDbMdbx::new(env, witness_db)
    }

    #[test]
    fn unpublished_code_hash_has_no_holder() {
        let ctx = setup_da_context();
        let hash = B256::from([0x11u8; 32]);
        assert_eq!(ctx.code_hash_holder(&hash).unwrap(), None);
    }

    #[test]
    fn mark_and_query_code_hash_holders() {
        let ctx = setup_da_context();
        let hash_a = B256::from([0xAAu8; 32]);
        let hash_b = B256::from([0xBBu8; 32]);
        let hash_c = B256::from([0xCCu8; 32]);
        let holder_a = Address::from([0x0Au8; 20]);
        let holder_b = Address::from([0x0Bu8; 20]);

        ctx.mark_code_hashes_published(&[(hash_a, holder_a), (hash_b, holder_b)])
            .unwrap();

        assert_eq!(ctx.code_hash_holder(&hash_a).unwrap(), Some(holder_a));
        assert_eq!(ctx.code_hash_holder(&hash_b).unwrap(), Some(holder_b));
        assert_eq!(ctx.code_hash_holder(&hash_c).unwrap(), None);
    }

    #[test]
    fn mark_published_keeps_the_first_holder_and_empty_is_noop() {
        let ctx = setup_da_context();
        let hash = B256::from([0x11u8; 32]);
        let first = Address::from([0x01u8; 20]);
        let second = Address::from([0x02u8; 20]);

        ctx.mark_code_hashes_published(&[]).unwrap();
        ctx.mark_code_hashes_published(&[(hash, first)]).unwrap();
        ctx.mark_code_hashes_published(&[(hash, second)]).unwrap();

        assert_eq!(ctx.code_hash_holder(&hash).unwrap(), Some(first));
    }

    /// Rows written before holders were recorded carry an empty value. They
    /// read as no known holder, and a later batch fills the holder in.
    #[test]
    fn legacy_row_without_holder_reads_as_unknown_and_takes_a_holder() {
        let ctx = setup_da_context();
        let hash = B256::from([0x11u8; 32]);
        let holder = Address::from([0x01u8; 20]);
        ctx.env
            .update(|w| w.put::<PublishedCodeHashSchema>(&hash, &None))
            .unwrap();

        assert_eq!(ctx.code_hash_holder(&hash).unwrap(), None);

        ctx.mark_code_hashes_published(&[(hash, holder)]).unwrap();
        assert_eq!(ctx.code_hash_holder(&hash).unwrap(), Some(holder));
    }

    #[test]
    fn update_da_filter_records_accounts_that_took_on_new_code() {
        let env = temp_env();
        let witness_db = Arc::new(WitnessDbMdbx::new(env.clone()));
        let ctx = DaContextDbMdbx::new(env, witness_db.clone());

        let deployed = address!("0x00000000000000000000000000000000000000d1");
        let unchanged = address!("0x00000000000000000000000000000000000000d2");
        let eoa = address!("0x00000000000000000000000000000000000000e1");
        let code_hash = B256::from([0xC0u8; 32]);
        let kept_code_hash = B256::from([0xC1u8; 32]);
        let snapshot = |code_hash| AccountSnapshot {
            balance: U256::ZERO,
            nonce: 1,
            code_hash,
        };

        let mut diff = BlockStateChanges::default();
        diff.accounts.insert(
            deployed,
            BlockAccountChange {
                original: None,
                current: Some(snapshot(code_hash)),
            },
        );
        diff.accounts.insert(
            unchanged,
            BlockAccountChange {
                original: Some(snapshot(kept_code_hash)),
                current: Some(snapshot(kept_code_hash)),
            },
        );
        diff.accounts.insert(
            eoa,
            BlockAccountChange {
                original: None,
                current: Some(snapshot(KECCAK_EMPTY)),
            },
        );
        // Code left behind by a contract created and destroyed in one
        // transaction: deployed, but held by no account.
        let orphan_code_hash = B256::from([0xC2u8; 32]);
        diff.deployed_bytecodes
            .insert(orphan_code_hash, Bytes::from_static(&[0x00]));
        witness_db.put_state_diff(BLOCK_HASH_ONE, 1, &diff).unwrap();

        ctx.update_da_filter(&[BLOCK_HASH_ONE]).unwrap();

        assert_eq!(ctx.code_hash_holder(&code_hash).unwrap(), Some(deployed));
        assert_eq!(ctx.code_hash_holder(&kept_code_hash).unwrap(), None);
        assert_eq!(ctx.code_hash_holder(&KECCAK_EMPTY).unwrap(), None);
        assert_eq!(ctx.code_hash_holder(&orphan_code_hash).unwrap(), None);
    }
}
