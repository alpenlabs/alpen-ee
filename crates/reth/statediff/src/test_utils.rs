//! Shared test helpers for state-diff fixture construction and canonical-state oracles.
//!
//! This module centralizes deterministic builders used across `alpen-reth-statediff`
//! tests, including block-diff assembly helpers and canonical MPT-derived state
//! computations for reconstruction oracle checks.

use std::collections::BTreeMap;

use alloy_primitives::Bytes;
use alloy_trie::{TrieAccount, EMPTY_ROOT_HASH};
use revm_primitives::{alloy_primitives::keccak256, Address, B256, U256};
use rsp_mpt::EthereumState;

use crate::{
    batch::{BatchBuilder, BatchStateDiff},
    block::{AccountSnapshot, BlockAccountChange, BlockStateChanges},
    reconstruct::is_account_empty,
};

/// Canonical per-account storage view used by test oracles.
pub(crate) type AccountStorage = BTreeMap<U256, U256>;

/// Returns a deterministic address derived from a single-byte seed.
pub(crate) fn addr(seed: u8) -> Address {
    Address::from([seed; 20])
}

/// Returns a deterministic `B256` hash derived from a single-byte seed.
pub(crate) fn hash(seed: u8) -> B256 {
    B256::from([seed; 32])
}

/// Returns a storage-slot key from a small integer.
pub(crate) fn slot(value: u64) -> U256 {
    U256::from(value)
}

/// Returns a storage or balance value from a small integer.
pub(crate) fn value(value: u64) -> U256 {
    U256::from(value)
}

/// Copies bytecode into owned bytes for block-diff fixtures.
pub(crate) fn bytecode(bytes: &[u8]) -> Bytes {
    Bytes::copy_from_slice(bytes)
}

/// Builds a compact account snapshot for test fixtures.
pub(crate) fn snapshot(balance: u64, nonce: u64, code_hash: B256) -> AccountSnapshot {
    AccountSnapshot {
        balance: U256::from(balance),
        nonce,
        code_hash,
    }
}

/// Inserts an account-level change into a block diff fixture.
pub(crate) fn account_change(
    diff: &mut BlockStateChanges,
    address: Address,
    original: Option<AccountSnapshot>,
    current: Option<AccountSnapshot>,
) {
    diff.accounts
        .insert(address, BlockAccountChange { original, current });
}

/// Inserts a storage-slot change into a block diff fixture.
pub(crate) fn storage_change(
    diff: &mut BlockStateChanges,
    address: Address,
    slot_key: U256,
    original: U256,
    current: U256,
) {
    diff.storage
        .entry(address)
        .or_default()
        .slots
        .insert(slot_key, (original, current));
}

/// Records deployed bytecode in a block diff fixture.
pub(crate) fn deployed_bytecode(
    diff: &mut BlockStateChanges,
    code_hash: B256,
    deployed_bytecode: Bytes,
) {
    diff.deployed_bytecodes.insert(code_hash, deployed_bytecode);
}

/// Creates an empty per-block state diff fixture.
pub(crate) fn block_diff() -> BlockStateChanges {
    BlockStateChanges::new()
}

/// Aggregates a sequence of block diffs into a single batch diff.
pub(crate) fn batch_diff(blocks: &[BlockStateChanges]) -> BatchStateDiff {
    let mut builder = BatchBuilder::new();
    for block in blocks {
        builder.apply_block(block);
    }
    builder.build()
}

/// Canonical account and storage state used by reconstruction oracles.
#[derive(Clone, Debug, Default)]
pub(crate) struct CanonicalState {
    /// Final account records keyed by address.
    pub(crate) accounts: BTreeMap<Address, TrieAccount>,
    /// Final storage contents keyed by address and slot.
    pub(crate) storage: BTreeMap<Address, AccountStorage>,
}

impl CanonicalState {
    /// Creates an empty canonical state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a canonical account entry.
    pub(crate) fn with_account(mut self, address: Address, account: TrieAccount) -> Self {
        self.accounts.insert(address, account);
        self
    }

    /// Sets a canonical storage slot value for an account.
    pub(crate) fn set_storage_slot(
        mut self,
        address: Address,
        slot_key: U256,
        slot_value: U256,
    ) -> Self {
        self.storage
            .entry(address)
            .or_default()
            .insert(slot_key, slot_value);
        self
    }

    /// Removes a canonical storage slot and prunes empty account storage maps.
    pub(crate) fn remove_storage_slot(mut self, address: Address, slot_key: U256) -> Self {
        if let Some(account_storage) = self.storage.get_mut(&address) {
            account_storage.remove(&slot_key);
            if account_storage.is_empty() {
                self.storage.remove(&address);
            }
        }
        self
    }
}

/// Builds a canonical `TrieAccount` with an empty storage root placeholder.
pub(crate) fn state_account(balance: u64, nonce: u64, code_hash: B256) -> TrieAccount {
    TrieAccount {
        nonce,
        balance: U256::from(balance),
        storage_root: EMPTY_ROOT_HASH,
        code_hash,
    }
}

/// Builds the canonical [`EthereumState`] for the provided state view.
///
/// Storage tries are keyed by hashed address, matching the shape the
/// reconstruction code expects. Accounts that are empty under EIP-161 are left
/// out of the state trie.
pub(crate) fn canonical_ethereum_state(
    state: &CanonicalState,
) -> Result<EthereumState, rsp_mpt::Error> {
    let mut ethereum_state = EthereumState {
        state_trie: Default::default(),
        storage_tries: Default::default(),
    };

    for (address, storage) in &state.storage {
        let storage_trie = ethereum_state
            .storage_tries
            .entry(keccak256(address))
            .or_default();
        for (slot_key, slot_value) in storage {
            if slot_value.is_zero() {
                continue;
            }

            let slot_trie_path = keccak256(slot_key.to_be_bytes::<32>());
            storage_trie.insert_rlp(slot_trie_path.as_slice(), *slot_value)?;
        }
    }

    for (address, account) in canonical_accounts(state)? {
        if is_account_empty(&account) {
            continue;
        }
        ethereum_state
            .state_trie
            .insert_rlp(keccak256(address).as_slice(), account)?;
    }

    Ok(ethereum_state)
}

/// Recomputes canonical accounts with storage roots derived from canonical storage.
pub(crate) fn canonical_accounts(
    state: &CanonicalState,
) -> Result<BTreeMap<Address, TrieAccount>, rsp_mpt::Error> {
    let storage_roots = canonical_storage_roots(state)?;
    let mut accounts = BTreeMap::new();

    for (address, account) in &state.accounts {
        let mut account = *account;
        account.storage_root = storage_roots
            .get(address)
            .copied()
            .unwrap_or(EMPTY_ROOT_HASH);
        accounts.insert(*address, account);
    }

    Ok(accounts)
}

/// Computes the canonical storage root of every account present in the state view.
fn canonical_storage_roots(
    state: &CanonicalState,
) -> Result<BTreeMap<Address, B256>, rsp_mpt::Error> {
    let mut tries = EthereumState {
        state_trie: Default::default(),
        storage_tries: Default::default(),
    };
    let mut roots = BTreeMap::new();

    for (address, storage) in &state.storage {
        let hashed_addr = keccak256(address);
        let storage_trie = tries.storage_tries.entry(hashed_addr).or_default();
        for (slot_key, slot_value) in storage {
            if slot_value.is_zero() {
                continue;
            }

            let slot_trie_path = keccak256(slot_key.to_be_bytes::<32>());
            storage_trie.insert_rlp(slot_trie_path.as_slice(), *slot_value)?;
        }
        roots.insert(*address, storage_trie.hash());
    }

    Ok(roots)
}

/// Computes the canonical global state root for the provided state view.
pub(crate) fn canonical_state_root(state: &CanonicalState) -> Result<B256, rsp_mpt::Error> {
    Ok(canonical_ethereum_state(state)?.state_root())
}
