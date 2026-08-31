use std::{path::Path, sync::Arc};

use alpen_db_store_mdbx::{DbError as MdbxError, MdbxConfig, MdbxEnv};
use alpen_reth_statediff::BlockStateChanges;
use revm_primitives::alloy_primitives::B256;
use tracing::warn;

use super::schema::{
    witness_tables, BlockHashByNumber, BlockStateChangesSchema, PublishedCodeHashSchema,
};
use crate::{errors::DbError, DbResult, EeDaContext, StateDiffProvider, StateDiffStore};

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
pub struct EeDaContextDbMdbx<S> {
    env: Arc<MdbxEnv>,
    state_diff_provider: Arc<S>,
}

impl<S> EeDaContextDbMdbx<S> {
    /// Wraps an environment whose tables include the published-code-hash table,
    /// reading state diffs through `state_diff_provider`.
    pub fn new(env: Arc<MdbxEnv>, state_diff_provider: Arc<S>) -> Self {
        Self {
            env,
            state_diff_provider,
        }
    }
}

impl<S: StateDiffProvider + 'static> EeDaContextDbMdbx<S> {
    /// Collects deployed bytecodes from block state diffs and marks them in the
    /// filter so future batches can omit them.
    fn update_bytecode_filter(&self, block_hashes: &[B256]) -> DbResult<()> {
        let mut code_hashes = Vec::new();
        for block_hash in block_hashes {
            match self.state_diff_provider.get_state_diff_by_hash(*block_hash) {
                Ok(Some(diff)) => code_hashes.extend(diff.deployed_bytecodes.keys().copied()),
                Ok(None) => {}
                Err(e) => {
                    warn!(%block_hash, error = %e, "failed to fetch state diff for block, skipping");
                }
            }
        }
        if !code_hashes.is_empty() {
            self.mark_code_hashes_published(&code_hashes)?;
        }
        Ok(())
    }
}

impl<S: StateDiffProvider + 'static> EeDaContext for EeDaContextDbMdbx<S> {
    fn is_code_hash_published(&self, code_hash: &B256) -> DbResult<bool> {
        let exists = self
            .env
            .view(|r| r.get::<PublishedCodeHashSchema>(code_hash))
            .map_err(to_db_error)?;
        Ok(exists.is_some())
    }

    fn mark_code_hashes_published(&self, code_hashes: &[B256]) -> DbResult<()> {
        self.env
            .update(|w| {
                for hash in code_hashes {
                    w.put::<PublishedCodeHashSchema>(hash, &Vec::new())?;
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

    use alpen_reth_statediff::{
        AccountSnapshot, BlockAccountChange, BlockStateChanges, BlockStorageDiff,
    };
    use revm_primitives::{address, fixed_bytes, FixedBytes, KECCAK_EMPTY, U256};

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

    fn setup_da_context() -> EeDaContextDbMdbx<WitnessDbMdbx> {
        let env = temp_env();
        let witness_db = Arc::new(WitnessDbMdbx::new(env.clone()));
        EeDaContextDbMdbx::new(env, witness_db)
    }

    #[test]
    fn unpublished_code_hash_returns_false() {
        let ctx = setup_da_context();
        let hash = B256::from([0x11u8; 32]);
        assert!(!ctx.is_code_hash_published(&hash).unwrap());
    }

    #[test]
    fn mark_and_query_published_code_hashes() {
        let ctx = setup_da_context();
        let hash_a = B256::from([0xAAu8; 32]);
        let hash_b = B256::from([0xBBu8; 32]);
        let hash_c = B256::from([0xCCu8; 32]);

        ctx.mark_code_hashes_published(&[hash_a, hash_b]).unwrap();

        assert!(ctx.is_code_hash_published(&hash_a).unwrap());
        assert!(ctx.is_code_hash_published(&hash_b).unwrap());
        assert!(!ctx.is_code_hash_published(&hash_c).unwrap());
    }

    #[test]
    fn mark_published_is_idempotent_and_empty_is_noop() {
        let ctx = setup_da_context();
        let hash = B256::from([0x11u8; 32]);

        ctx.mark_code_hashes_published(&[]).unwrap();
        ctx.mark_code_hashes_published(&[hash]).unwrap();
        ctx.mark_code_hashes_published(&[hash]).unwrap();

        assert!(ctx.is_code_hash_published(&hash).unwrap());
    }
}
