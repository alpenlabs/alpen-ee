//! In-memory stand-in for the node's accessed-state store.

use std::{collections::HashMap, sync::Mutex};

use alpen_common::{AccessedStateRecord, AccessedStateStore, StorageError};
use alpen_exex::BytecodeEntry;
use async_trait::async_trait;
use strata_acct_types::Hash;

/// Holds the records the accessed-state exex would have written, so the
/// production range witness extractor can read them.
#[derive(Debug, Default)]
pub(crate) struct MemAccessedStateStore {
    records: Mutex<HashMap<Hash, AccessedStateRecord>>,
    bytecodes: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl MemAccessedStateStore {
    pub(crate) fn insert(
        &self,
        block_id: Hash,
        record: AccessedStateRecord,
        bytecodes: Vec<BytecodeEntry>,
    ) {
        self.records
            .lock()
            .expect("store lock")
            .insert(block_id, record);
        self.bytecodes.lock().expect("store lock").extend(bytecodes);
    }
}

#[async_trait]
impl AccessedStateStore for MemAccessedStateStore {
    async fn put_block_accessed_state(
        &self,
        block_id: Hash,
        record: AccessedStateRecord,
    ) -> Result<(), StorageError> {
        self.insert(block_id, record, Vec::new());
        Ok(())
    }

    async fn get_block_accessed_state(
        &self,
        block_id: Hash,
    ) -> Result<Option<AccessedStateRecord>, StorageError> {
        Ok(self
            .records
            .lock()
            .expect("store lock")
            .get(&block_id)
            .cloned())
    }

    async fn del_block_accessed_state(&self, block_id: Hash) -> Result<(), StorageError> {
        self.records.lock().expect("store lock").remove(&block_id);
        Ok(())
    }

    async fn put_bytecode(&self, code_hash: Hash, code: Vec<u8>) -> Result<(), StorageError> {
        self.bytecodes
            .lock()
            .expect("store lock")
            .insert(code_hash, code);
        Ok(())
    }

    async fn get_bytecode(&self, code_hash: Hash) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self
            .bytecodes
            .lock()
            .expect("store lock")
            .get(&code_hash)
            .cloned())
    }
}
