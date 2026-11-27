//! Witness database implementation for EVM execution.
//!
//! This module provides a custom DatabaseRef implementation that avoids unnecessary
//! clones by holding only references to pre-computed data in EvmPartialState.

use std::{cell::RefCell, collections::BTreeMap, fmt};

use evm2::{bytecode::Bytecode as Evm2Bytecode, evm::AccountInfo as Evm2AccountInfo};
use reth_errors::ProviderError;
use reth_trie_common::TrieAccount;
use revm::{
    primitives::{Address, B256, U256, keccak256},
    state::{AccountInfo, Bytecode},
};
use revm_primitives::map::HashMap;
use rsp_mpt::EthereumState;

/// Custom witness database that implements DatabaseRef using only references.
///
/// Uses lazy hashing with caching: addresses and storage keys are hashed once on
/// first access, then cached for reuse. This avoids redundant hashing across
/// multiple accesses within a block and across blocks in batch execution.
pub struct WitnessDB<'a> {
    /// Reference to the sparse Merkle Patricia Trie state
    ethereum_state: &'a EthereumState,
    /// Reference to pre-computed block hashes map (no allocation needed)
    block_hashes: &'a HashMap<u64, B256>,
    /// Reference to bytecode map (no intermediate HashMap creation)
    bytecodes: &'a BTreeMap<B256, Bytecode>,
    /// Cache for hashed addresses (lazy computed, then reused)
    address_hash_cache: RefCell<HashMap<Address, B256>>,
    /// Cache for hashed storage keys (lazy computed, then reused)
    storage_key_cache: RefCell<HashMap<U256, B256>>,
}

impl<'a> fmt::Debug for WitnessDB<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WitnessDB")
            .field("ethereum_state", &self.ethereum_state)
            .field("block_hashes", &self.block_hashes)
            .field("bytecodes_count", &self.bytecodes.len())
            .field(
                "address_cache_size",
                &self.address_hash_cache.borrow().len(),
            )
            .field(
                "storage_key_cache_size",
                &self.storage_key_cache.borrow().len(),
            )
            .finish()
    }
}

impl<'a> WitnessDB<'a> {
    /// Creates a new WitnessDB with references to pre-computed data.
    ///
    /// Hash caches are initialized empty and populated lazily on first access.
    pub fn new(
        ethereum_state: &'a EthereumState,
        block_hashes: &'a HashMap<u64, B256>,
        bytecodes: &'a BTreeMap<B256, Bytecode>,
    ) -> Self {
        Self {
            ethereum_state,
            block_hashes,
            bytecodes,
            address_hash_cache: RefCell::new(HashMap::with_hasher(Default::default())),
            storage_key_cache: RefCell::new(HashMap::with_hasher(Default::default())),
        }
    }
}

impl WitnessDB<'_> {
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, ProviderError> {
        // Check if address hash is cached, otherwise compute and cache it
        let hashed_address = {
            let cache = self.address_hash_cache.borrow();
            if let Some(&cached_hash) = cache.get(&address) {
                // Reuse cached hash - avoids redundant keccak256 computation
                cached_hash
            } else {
                // Not cached - compute hash once and store for future accesses
                drop(cache); // Release borrow before mutable borrow
                let hash = keccak256(address);
                self.address_hash_cache.borrow_mut().insert(address, hash);
                hash
            }
        };

        // Query the state trie for the account using cached/new hash
        let account_in_trie = self
            .ethereum_state
            .state_trie
            .get_rlp::<TrieAccount>(hashed_address.as_slice())
            .unwrap();

        // Convert TrieAccount to AccountInfo
        let account = account_in_trie.map(|account_in_trie| AccountInfo {
            balance: account_in_trie.balance,
            nonce: account_in_trie.nonce,
            code_hash: account_in_trie.code_hash,
            account_id: None,
            code: None,
        });

        Ok(account)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, ProviderError> {
        // Check if address hash is cached, otherwise compute and cache it
        let hashed_address = {
            let cache = self.address_hash_cache.borrow();
            if let Some(&cached_hash) = cache.get(&address) {
                // Reuse cached hash
                cached_hash
            } else {
                // Not cached - compute hash once and store
                drop(cache);
                let hash = keccak256(address);
                self.address_hash_cache.borrow_mut().insert(address, hash);
                hash
            }
        };

        // Fail rather than panic. A missing trie says the witness is
        // incomplete, which is a fact about the input, not a bug here.
        let storage_trie = self
            .ethereum_state
            .storage_tries
            .get(hashed_address.as_slice())
            .ok_or_else(|| {
                ProviderError::TrieWitnessError(format!(
                    "missing storage trie for account {address}"
                ))
            })?;

        // Check if storage key hash is cached, otherwise compute and cache it
        let hashed_index = {
            let cache = self.storage_key_cache.borrow();
            if let Some(&cached_hash) = cache.get(&index) {
                // Reuse cached hash
                cached_hash
            } else {
                // Not cached - compute hash once and store
                drop(cache);
                let hash = keccak256(index.to_be_bytes::<32>());
                self.storage_key_cache.borrow_mut().insert(index, hash);
                hash
            }
        };

        // Query the storage trie using cached/new hashes
        Ok(storage_trie
            .get_rlp::<U256>(hashed_index.as_slice())
            .expect("Can get from MPT")
            .unwrap_or_default())
    }

    pub(crate) fn block_hash_ref(&self, number: u64) -> Result<B256, ProviderError> {
        // Fail rather than defaulting to zero. The EVM only asks for numbers
        // it is prepared to use (revm returns zero for out-of-range ones
        // without consulting the database), so a missing entry means the
        // witness is incomplete -- and defaulting would let whoever built it
        // choose zero over the real hash by leaving the header out.
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or(ProviderError::HeaderNotFound(number.into()))
    }
}

impl evm2::evm::Database for WitnessDB<'_> {
    type Error = ProviderError;

    fn is_fatal(_error: &Self::Error) -> bool {
        // The backing input is the supplied witness, not a node database.
        false
    }

    fn get_account(&mut self, address: &Address) -> Result<Option<Evm2AccountInfo>, ProviderError> {
        Ok(self.basic_ref(*address)?.map(|account| {
            Evm2AccountInfo::new(account.balance, account.nonce, account.code_hash, None)
        }))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Evm2Bytecode, ProviderError> {
        let code = self.bytecodes.get(code_hash).ok_or_else(|| {
            ProviderError::TrieWitnessError(format!("missing bytecode {code_hash}"))
        })?;
        Evm2Bytecode::new_raw_checked(code.original_bytes()).map_err(|err| {
            ProviderError::TrieWitnessError(format!("invalid bytecode {code_hash}: {err}"))
        })
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, ProviderError> {
        self.storage_ref(*address, *key)
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, ProviderError> {
        let number: u64 = (*number).try_into().map_err(|_| {
            ProviderError::TrieWitnessError(format!("out-of-range block number {number}"))
        })?;
        self.block_hash_ref(number)
    }
}
