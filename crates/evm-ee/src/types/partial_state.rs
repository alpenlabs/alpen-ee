//! EVM partial state implementation.

use std::collections::BTreeMap;

use alloy_consensus::{BlockHeader, Header, Sealable, Sealed, constants::EMPTY_ROOT_HASH};
use alloy_rpc_types_debug::ExecutionWitness;
use itertools::Itertools;
use reth_trie::{HashedPostState, TrieAccount};
use revm::state::Bytecode;
use revm_primitives::{B256, Bytes, keccak256, map::HashMap};
use rsp_mpt::EthereumState;
use strata_acct_types::Hash;
use strata_codec::{Codec, CodecError};
use strata_ee_acct_types::{EnvError, EnvResult, ExecPartialState};

use crate::{
    codec_shims::{
        decode_bytes_with_length, decode_ethereum_state, decode_rlp_with_length,
        encode_bytes_with_length, encode_ethereum_state, encode_rlp_with_length,
    },
    types::{EvmWriteBatch, WitnessDB},
};

/// Partial state for EVM block execution.
///
/// Contains the witness data needed to execute a block: the sparse Merkle Patricia Trie
/// state, contract bytecodes, and ancestor block headers for `BLOCKHASH` opcode support.
///
/// This struct pre-computes expensive operations (header hashing, block hash map) during
/// construction to avoid repeated work when preparing witness databases.
#[derive(Clone, Debug)]
pub struct EvmPartialState {
    /// The sparse Merkle Patricia Trie state from RSP
    ethereum_state: EthereumState,
    /// Contract bytecodes indexed by their hash for direct lookup during execution.
    /// BTreeMap is used (instead of HashMap) to ensure deterministic serialization order in Codec.
    bytecodes: BTreeMap<B256, Bytecode>,
    /// Ancestor block headers with pre-computed hashes, indexed by block number.
    /// Headers are sealed once during construction to avoid repeated hash computations.
    ancestor_headers: BTreeMap<u64, Sealed<Header>>,
    /// Pre-computed block hash lookup map for `BLOCKHASH` opcode.
    /// Built once during construction from sealed ancestor headers.
    block_hashes: HashMap<u64, B256>,
}

impl EvmPartialState {
    /// Creates a new EvmPartialState from an EthereumState with witness data.
    ///
    /// This performs expensive one-time operations optimized for zkVM execution:
    /// - Seals all ancestor headers (computes their hashes once)
    /// - Validates header chain integrity
    /// - Builds block_hashes lookup map once
    ///
    /// These operations are done once at construction to avoid repeated work
    /// during sequential block execution in zkVM.
    ///
    /// # Panics
    /// Panics if the header chain is invalid (block numbers or parent hashes don't match).
    pub fn new(
        ethereum_state: EthereumState,
        bytecodes: BTreeMap<B256, Bytecode>,
        ancestor_headers: Vec<Header>,
    ) -> Self {
        // Seal ancestor headers once (compute hashes) and index by block number
        let ancestor_headers: BTreeMap<u64, Sealed<Header>> = ancestor_headers
            .into_iter()
            .map(|header| {
                let block_num = header.number;
                (block_num, header.seal_slow())
            })
            .collect();

        let block_hashes = build_block_hashes(&ancestor_headers);

        Self {
            ethereum_state,
            bytecodes,
            ancestor_headers,
            block_hashes,
        }
    }

    /// Builds an [`EvmPartialState`] from raw execution-witness parts, anchored
    /// at `pre_state_root`.
    ///
    /// `witness_state` is the bag of RLP-encoded MPT nodes (the
    /// [`ExecutionWitness::state`] format); `codes` are the loaded bytecodes
    /// (keyed here by their keccak hash); `ancestor_headers` back the
    /// `BLOCKHASH` opcode. The sparse trie is reconstructed via rsp's
    /// `EthereumState::from_execution_witness`, which resolves the node bag
    /// against `pre_state_root` — purely in-memory, with no historical-state
    /// access. This is the entry point for assembling one chunk-level state
    /// from the union of a chunk's per-block witness node bags.
    ///
    /// Host-side only (trie reconstruction); the guest consumes the encoded
    /// result.
    pub fn from_witness_parts(
        witness_state: Vec<Vec<u8>>,
        pre_state_root: B256,
        codes: Vec<Vec<u8>>,
        ancestor_headers: Vec<Header>,
    ) -> Self {
        let witness = ExecutionWitness {
            state: witness_state.into_iter().map(Bytes::from).collect(),
            ..Default::default()
        };
        let ethereum_state = EthereumState::from_execution_witness(&witness, pre_state_root);

        let bytecodes = codes
            .into_iter()
            .map(|code| {
                let bytes = Bytes::from(code);
                (keccak256(&bytes), Bytecode::new_raw(bytes))
            })
            .collect();

        Self::new(ethereum_state, bytecodes, ancestor_headers)
    }

    /// Gets a reference to the underlying EthereumState.
    pub fn ethereum_state(&self) -> &EthereumState {
        &self.ethereum_state
    }

    /// Gets a mutable reference to the underlying EthereumState.
    pub fn ethereum_state_mut(&mut self) -> &mut EthereumState {
        &mut self.ethereum_state
    }

    /// Gets a reference to the bytecodes map.
    pub fn bytecodes(&self) -> &BTreeMap<B256, Bytecode> {
        &self.bytecodes
    }

    /// Gets a reference to the ancestor headers map (with pre-computed hashes).
    pub fn ancestor_headers(&self) -> &BTreeMap<u64, Sealed<Header>> {
        &self.ancestor_headers
    }

    /// Gets a reference to the pre-computed block hashes map.
    pub fn block_hashes(&self) -> &HashMap<u64, B256> {
        &self.block_hashes
    }

    // NOTE: same comment as `add_executed_block`
    pub fn add_bytecodes(&mut self, new_bytecodes: BTreeMap<B256, Bytecode>) {
        for (hash, bytecode) in new_bytecodes {
            self.bytecodes.entry(hash).or_insert(bytecode);
        }
    }

    /// Adds a newly executed block's header to the witness state.
    ///
    /// This is called after executing a block in a batch to make its hash
    /// available for `BLOCKHASH` opcode in subsequent blocks.
    // NOTE: not sure we we should be adding this in proof generation flow. Looks like we can
    // prepare all of this for whole batch while generating witness.
    pub fn add_executed_block(&mut self, header: Header) {
        let sealed = header.seal_slow(); // Hash once
        let block_num = sealed.number();
        let block_hash = sealed.hash();

        // Add to both maps for subsequent block execution
        self.ancestor_headers.insert(block_num, sealed);
        self.block_hashes.insert(block_num, block_hash);
    }

    /// Prepares witness database for block execution.
    ///
    /// Note: Current header validation should be done externally before calling this method.
    pub fn create_witness_db<'a>(&'a self) -> WitnessDB<'a> {
        // Simply create a view with references to pre-computed data
        WitnessDB::new(&self.ethereum_state, &self.block_hashes, &self.bytecodes)
    }

    /// Merges a write batch into this state by applying the hashed post state changes.
    ///
    /// This updates the internal EthereumState with the changes from the write batch.
    /// It fails when the witness is missing a storage trie the merge would need.
    pub fn merge_write_batch(&mut self, wb: &EvmWriteBatch) -> EnvResult<()> {
        let post_state = wb.hashed_post_state();
        require_storage_tries_for_writes(&self.ethereum_state, post_state)?;
        self.ethereum_state.update(post_state);
        Ok(())
    }
}

/// Requires a storage trie for every account the batch rewrites whose leaf
/// commits to non-empty storage.
///
/// [`EthereumState::update`] recomputes a written account's storage root from
/// the tries carried alongside the state trie, falling back to an empty trie
/// when the account has none. A witness that leaves a trie out would therefore
/// erase that account's storage instead of preserving it, and the account
/// needs no storage access for that to happen: a plain balance change is
/// enough. Refuse the merge instead of building a state the witness never
/// justified.
///
/// Storage the block wipes is exempt, since a wipe clears the trie anyway.
fn require_storage_tries_for_writes(
    state: &EthereumState,
    post_state: &HashedPostState,
) -> EnvResult<()> {
    for (hashed_address, account) in post_state.accounts.iter() {
        // Deleting an account takes its storage with it.
        if account.is_none() {
            continue;
        }

        if state.storage_tries.contains_key(hashed_address) {
            continue;
        }

        if post_state
            .storages
            .get(hashed_address)
            .is_some_and(|storage| storage.wiped)
        {
            continue;
        }

        let storage_root = state
            .state_trie
            .get_rlp::<TrieAccount>(hashed_address.as_slice())
            .map_err(|_| EnvError::InsufficientPartialState)?
            .map_or(EMPTY_ROOT_HASH, |account| account.storage_root);

        if storage_root != EMPTY_ROOT_HASH {
            return Err(EnvError::InsufficientPartialState);
        }
    }

    Ok(())
}

impl ExecPartialState for EvmPartialState {
    fn compute_state_root(&self) -> EnvResult<Hash> {
        let state_root = self.ethereum_state.state_root();
        Ok(state_root.0.into())
    }
}

impl Codec for EvmPartialState {
    fn encode(&self, enc: &mut impl strata_codec::Encoder) -> Result<(), CodecError> {
        // Encode EthereumState using custom deterministic encoding
        encode_ethereum_state(&self.ethereum_state, enc)?;

        // Encode bytecodes count
        (self.bytecodes.len() as u32).encode(enc)?;
        // Encode only the code. The map key is its hash, which the decoder
        // recomputes rather than trusting -- see `decode`.
        for bytecode in self.bytecodes.values() {
            encode_bytes_with_length(&bytecode.original_bytes(), enc)?;
        }

        // Encode ancestor headers count
        (self.ancestor_headers.len() as u32).encode(enc)?;
        // Encode only the header. Its hash is likewise recomputed on decode.
        for sealed_header in self.ancestor_headers.values() {
            encode_rlp_with_length(sealed_header.inner(), enc)?;
        }

        Ok(())
    }

    fn decode(dec: &mut impl strata_codec::Decoder) -> Result<Self, CodecError> {
        // Decode EthereumState using custom deterministic decoding
        let ethereum_state = decode_ethereum_state(dec)?;

        // Decode bytecodes, keying each one by the hash of the code itself.
        //
        // The key is what the EVM looks code up by, and the account's
        // `code_hash` in the trie is the only thing committing to it. Taking
        // the key from the encoding instead would let a prover serve any code
        // it likes for a deployed contract, since the trie never commits to
        // the code behind the hash.
        let bytecodes_count = u32::decode(dec)? as usize;
        let mut bytecodes = BTreeMap::new();
        for _ in 0..bytecodes_count {
            let bytes = decode_bytes_with_length(dec)?;
            let bytecode = Bytecode::new_raw_checked(bytes.into())
                .map_err(|_| CodecError::MalformedField("Bytecode decode failed"))?;

            bytecodes.insert(bytecode.hash_slow(), bytecode);
        }

        // Decode ancestor headers, sealing each with its own computed hash for
        // the same reason: these hashes are what `BLOCKHASH` returns.
        let headers_count = u32::decode(dec)? as usize;
        let mut ancestor_headers_sealed = Vec::with_capacity(headers_count);
        for _ in 0..headers_count {
            let header: Header = decode_rlp_with_length(dec)?;
            ancestor_headers_sealed.push(header.seal_slow());
        }

        // Build ancestor_headers BTreeMap directly from sealed headers
        let ancestor_headers: BTreeMap<u64, Sealed<Header>> = ancestor_headers_sealed
            .into_iter()
            .map(|sealed| (sealed.number(), sealed))
            .collect();

        let block_hashes = build_block_hashes(&ancestor_headers);

        Ok(Self {
            ethereum_state,
            bytecodes,
            ancestor_headers,
            block_hashes,
        })
    }
}

/// Validates ancestor header continuity and builds the `BLOCKHASH` lookup map.
///
/// The lookup map stores every ancestor block number with that header's own hash,
/// matching EVM `BLOCKHASH(n)` semantics.
fn build_block_hashes(ancestor_headers: &BTreeMap<u64, Sealed<Header>>) -> HashMap<u64, B256> {
    for (parent_sealed, child_sealed) in ancestor_headers.values().tuple_windows() {
        assert_eq!(
            parent_sealed.number() + 1,
            child_sealed.number(),
            "Invalid header block number: expected {}, got {}",
            parent_sealed.number() + 1,
            child_sealed.number()
        );

        let parent_hash = parent_sealed.hash();
        assert_eq!(
            parent_hash,
            child_sealed.parent_hash(),
            "Invalid header parent hash: expected {}, got {}",
            parent_hash,
            child_sealed.parent_hash()
        );
    }

    ancestor_headers
        .values()
        .map(|sealed_header| (sealed_header.number(), sealed_header.hash()))
        .collect()
}
