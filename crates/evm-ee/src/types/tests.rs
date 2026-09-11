//! Tests for EVM types Codec implementations.

use std::{collections::BTreeMap, fs::read_to_string, path::PathBuf};

use alloy_consensus::{Header, Sealable, constants::EMPTY_ROOT_HASH};
use alloy_rpc_types_debug::ExecutionWitness;
use reth_primitives_traits::Account;
use reth_trie::{HashedPostState, HashedStorage, TrieAccount};
use revm::{DatabaseRef, state::Bytecode};
use revm_primitives::alloy_primitives::{Address, B256, Bloom, Bytes, U256, keccak256};
use rsp_client_executor::io::EthClientExecutorInput;
use rsp_mpt::EthereumState;
use serde::Deserialize;
use strata_codec::{decode_buf_exact, encode_to_vec};
use strata_ee_acct_types::ExecHeader;

use super::{EvmBlock, EvmBlockBody, EvmHeader, EvmPartialState, EvmWriteBatch};

#[derive(Deserialize)]
struct TestData {
    witness: EthClientExecutorInput,
}

/// Helper function to load witness test data from the canonical fixture
/// under test-utils/data/evm_ee.
fn load_witness_test_data() -> EthClientExecutorInput {
    let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test-utils/data/evm_ee/witness_params.json");

    let json_content = read_to_string(&test_data_path)
        .expect("Failed to read witness_params.json from test-utils/data/evm_ee");

    let test_data: TestData =
        serde_json::from_str(&json_content).expect("Failed to parse test data");

    test_data.witness
}

fn rehashed_fixture_bytecodes(bytecodes: Vec<Bytecode>) -> BTreeMap<B256, Bytecode> {
    // The RSP fixture stores bytecodes as a Vec without the original code-hash
    // keys. Re-hashing here preserves the old fixture behavior; production
    // range witnesses pass keyed bytecodes from AccessedStateGenerator.
    bytecodes
        .into_iter()
        .map(|bytecode| (bytecode.hash_slow(), bytecode))
        .collect()
}

/// Helper function to create a test header with realistic values
fn create_test_header() -> Header {
    use revm_primitives::alloy_primitives::B64;

    Header {
        parent_hash: B256::from([1u8; 32]),
        ommers_hash: B256::from([2u8; 32]),
        beneficiary: Address::from([3u8; 20]),
        state_root: B256::from([4u8; 32]),
        transactions_root: B256::from([5u8; 32]),
        receipts_root: B256::from([6u8; 32]),
        logs_bloom: Bloom::ZERO,
        difficulty: U256::from(12345u64),
        number: 1000u64,
        gas_limit: 36_000_000u64,
        gas_used: 21_000u64,
        timestamp: 1234567890u64,
        extra_data: Bytes::from(vec![7u8, 8u8, 9u8]),
        mix_hash: B256::from([10u8; 32]),
        nonce: B64::from([0, 0, 0, 0, 0, 0, 0, 42]),
        base_fee_per_gas: Some(1_000_000_000u64),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
    }
}

fn create_test_header_at(number: u64, parent_hash: B256) -> Header {
    let mut header = create_test_header();
    header.parent_hash = parent_hash;
    header.number = number;
    header.timestamp += number;
    header.extra_data = Bytes::from(number.to_be_bytes().to_vec());
    header
}

fn create_test_ancestor_headers() -> Vec<Header> {
    let parent = create_test_header_at(100, B256::from([42u8; 32]));
    let child = create_test_header_at(101, parent.clone().seal_slow().hash());
    let grandchild = create_test_header_at(102, child.clone().seal_slow().hash());

    vec![parent, child, grandchild]
}

fn assert_block_hashes_match_headers(partial_state: &EvmPartialState, headers: &[Header]) {
    let witness_db = partial_state.create_witness_db();

    assert_eq!(partial_state.block_hashes().len(), headers.len());

    for header in headers {
        let sealed = header.clone().seal_slow();

        assert_eq!(
            partial_state.block_hashes().get(&header.number).copied(),
            Some(sealed.hash())
        );
        assert_eq!(
            witness_db
                .block_hash_ref(header.number)
                .expect("block hash lookup must succeed"),
            sealed.hash()
        );
    }
}

#[test]
fn test_evm_header_codec_roundtrip() {
    let header = create_test_header();
    let evm_header = EvmHeader::new(header.clone());

    // Encode
    let encoded = encode_to_vec(&evm_header).expect("encode failed");

    // Decode
    let decoded: EvmHeader = decode_buf_exact(&encoded).expect("decode failed");

    // Verify round-trip
    assert_eq!(decoded.header(), &header);
}

#[test]
fn test_evm_header_codec_with_post_merge_fields() {
    let mut header = create_test_header();
    // Add post-merge (Shanghai/Cancun) fields
    header.withdrawals_root = Some(B256::from([11u8; 32]));
    header.blob_gas_used = Some(131072u64);
    header.excess_blob_gas = Some(0u64);
    header.parent_beacon_block_root = Some(B256::from([12u8; 32]));

    let evm_header = EvmHeader::new(header.clone());

    // Encode and decode
    let encoded = encode_to_vec(&evm_header).expect("encode failed");
    let decoded: EvmHeader = decode_buf_exact(&encoded).expect("decode failed");

    // Verify all optional fields preserved
    assert_eq!(decoded.header(), &header);
}

#[test]
fn test_evm_header_exec_header_trait() {
    let header = create_test_header();
    let evm_header = EvmHeader::new(header.clone());

    // Test ExecHeader trait methods
    assert_eq!(evm_header.get_state_root().0, header.state_root.0);
    assert_eq!(evm_header.compute_block_id().0, header.hash_slow().0);
    assert_eq!(evm_header.get_intrinsics().number(), header.number);
    assert_eq!(evm_header.block_number(), header.number);
}

#[test]
fn test_evm_block_body_codec_empty() {
    // Create an empty block body (no transactions, no withdrawals)
    let body = EvmBlockBody::new(vec![]);

    // Encode
    let encoded = encode_to_vec(&body).expect("encode failed");

    // Decode
    let decoded: EvmBlockBody = decode_buf_exact(&encoded).expect("decode failed");

    // Verify empty
    assert_eq!(decoded.transaction_count(), 0);
    assert!(decoded.transactions().is_empty());
    assert!(decoded.body().withdrawals.is_none());
}

#[test]
fn test_evm_block_body_codec_roundtrip() {
    // Load witness data and extract block body
    let witness = load_witness_test_data();

    use reth_primitives_traits::Block;
    let block_body = witness.current_block.body().clone();
    let body = EvmBlockBody::from_alloy_body(block_body.clone());

    // Encode
    let encoded = encode_to_vec(&body).expect("encode failed");

    // Decode
    let decoded: EvmBlockBody = decode_buf_exact(&encoded).expect("decode failed");

    // Verify the entire body matches (compares all transactions and withdrawals)
    assert_eq!(
        decoded.body(),
        body.body(),
        "Block body should match exactly"
    );
}

#[test]
fn test_evm_block_codec_roundtrip() {
    // Load witness data and construct block
    let witness = load_witness_test_data();

    use reth_primitives_traits::Block;
    let header = witness.current_block.header().clone();
    let evm_header = EvmHeader::new(header.clone());

    let block_body = witness.current_block.body().clone();
    let evm_body = EvmBlockBody::from_alloy_body(block_body);

    let block = EvmBlock::new(evm_header, evm_body);

    // Encode
    let encoded = encode_to_vec(&block).expect("encode failed");

    // Decode
    let decoded: EvmBlock = decode_buf_exact(&encoded).expect("decode failed");

    // Verify header matches
    assert_eq!(
        decoded.header().header(),
        block.header().header(),
        "Header should match exactly"
    );

    // Verify body matches (compares all transactions and withdrawals)
    assert_eq!(
        decoded.body().body(),
        block.body().body(),
        "Block body should match exactly"
    );
}

#[test]
fn test_evm_partial_state_codec_roundtrip() {
    let witness = load_witness_test_data();
    let partial_state = EvmPartialState::new(
        witness.parent_state,
        rehashed_fixture_bytecodes(witness.bytecodes),
        witness.ancestor_headers,
    );

    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    // Verify state root matches
    assert_eq!(
        decoded.ethereum_state().state_root(),
        partial_state.ethereum_state().state_root()
    );

    // Verify bytecode hashes match
    let original_hashes: Vec<_> = partial_state
        .bytecodes()
        .values()
        .map(|b| b.hash_slow())
        .collect();
    let decoded_hashes: Vec<_> = decoded
        .bytecodes()
        .values()
        .map(|b| b.hash_slow())
        .collect();
    assert_eq!(decoded_hashes, original_hashes);

    // Verify ancestor headers match
    assert_eq!(decoded.ancestor_headers(), partial_state.ancestor_headers());
}

#[test]
fn test_evm_partial_state_block_hashes_include_single_ancestor() {
    let witness = load_witness_test_data();
    let ancestor_headers = vec![
        create_test_ancestor_headers()
            .into_iter()
            .next()
            .expect("test ancestor"),
    ];
    let partial_state = EvmPartialState::new(
        witness.parent_state,
        BTreeMap::new(),
        ancestor_headers.clone(),
    );

    assert_block_hashes_match_headers(&partial_state, &ancestor_headers);
}

#[test]
fn test_evm_partial_state_block_hashes_include_all_ancestors() {
    let witness = load_witness_test_data();
    let ancestor_headers = create_test_ancestor_headers();
    let partial_state = EvmPartialState::new(
        witness.parent_state,
        BTreeMap::new(),
        ancestor_headers.clone(),
    );

    assert_block_hashes_match_headers(&partial_state, &ancestor_headers);
}

#[test]
fn test_evm_partial_state_block_hashes_survive_codec_roundtrip() {
    let witness = load_witness_test_data();
    let ancestor_headers = create_test_ancestor_headers();
    let partial_state = EvmPartialState::new(
        witness.parent_state,
        BTreeMap::new(),
        ancestor_headers.clone(),
    );

    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    assert_block_hashes_match_headers(&decoded, &ancestor_headers);
}

#[test]
#[should_panic(expected = "Invalid header block number")]
fn test_evm_partial_state_rejects_non_contiguous_ancestor_headers() {
    let witness = load_witness_test_data();
    let parent = create_test_header_at(100, B256::from([42u8; 32]));
    let child = create_test_header_at(102, parent.clone().seal_slow().hash());

    EvmPartialState::new(witness.parent_state, BTreeMap::new(), vec![parent, child]);
}

#[test]
#[should_panic(expected = "Invalid header parent hash")]
fn test_evm_partial_state_rejects_invalid_ancestor_parent_hash() {
    let witness = load_witness_test_data();
    let mut ancestor_headers = create_test_ancestor_headers();
    ancestor_headers[1].parent_hash = B256::from([99u8; 32]);

    EvmPartialState::new(witness.parent_state, BTreeMap::new(), ancestor_headers);
}

/// Verifies that a codec-roundtripped EvmPartialState can still execute blocks.
///
/// The state_trie encoding uses `alloy_rlp::Encodable` for MptNode, which calls
/// `reference_encode` on child nodes. Nodes encoding to >= 32 bytes are collapsed
/// into hash digests, losing the resolved trie data needed for execution.
#[test]
fn test_evm_partial_state_codec_roundtrip_execution() {
    use std::sync::Arc;

    use alpen_reth_evm::evm::AlpenEvmFactory;
    use reth_chainspec::ChainSpec;
    use reth_primitives_traits::Block as _;
    use strata_ee_acct_types::{ExecBlock, ExecPayload, ExecutionEnvironment};
    use strata_ee_chain_types::ExecInputs;

    use crate::EvmExecutionEnvironment;

    let witness = load_witness_test_data();

    // Build partial state and verify execution works on the original.
    let partial_state = EvmPartialState::new(
        witness.parent_state,
        rehashed_fixture_bytecodes(witness.bytecodes),
        witness.ancestor_headers,
    );

    let header = witness.current_block.header().clone();
    let body = EvmBlockBody::from_alloy_body(witness.current_block.body().clone());
    let block = EvmBlock::new(EvmHeader::new(header.clone()), body);

    let chain_spec: Arc<ChainSpec> = Arc::new((&witness.genesis).try_into().unwrap());
    let ee = EvmExecutionEnvironment::new(chain_spec, AlpenEvmFactory::default());
    let intrinsics = block.get_header().get_intrinsics();
    let payload = ExecPayload::new(&intrinsics, block.get_body());
    let inputs = ExecInputs::new_empty();

    // Original state executes fine.
    ee.execute_block_body(&partial_state, &payload, &inputs)
        .expect("execution on original state should succeed");

    // Roundtrip through codec.
    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    // Decoded state should also execute — this fails if trie nodes are lost.
    ee.execute_block_body(&decoded, &payload, &inputs)
        .expect("execution on codec-roundtripped state should succeed");
}

#[test]
fn test_evm_write_batch_codec_roundtrip() {
    use reth_trie::HashedPostState;

    use super::EvmWriteBatch;

    let hashed_post_state = HashedPostState::default();
    let write_batch = EvmWriteBatch::new(hashed_post_state);

    // Encode
    let encoded = encode_to_vec(&write_batch).expect("encode failed");

    // Decode
    let decoded: EvmWriteBatch = decode_buf_exact(&encoded).expect("decode failed");

    let reencoded = encode_to_vec(&decoded).expect("re-encode failed");
    assert_eq!(reencoded, encoded);
}

/// Builds a state holding a single account, optionally with one storage slot
/// set. The account's storage root always matches the storage trie, so this is
/// a well-formed witness.
fn single_account_state(address: Address, slot: Option<(U256, U256)>) -> EthereumState {
    // An empty trie: the null node's RLP hashes to the empty root.
    let witness = ExecutionWitness {
        state: vec![Bytes::from_static(&[0x80])],
        ..Default::default()
    };
    let mut state = EthereumState::from_execution_witness(&witness, EMPTY_ROOT_HASH);

    let hashed_address = keccak256(address);
    let mut post_state = HashedPostState::default();
    post_state.accounts.insert(
        hashed_address,
        Some(Account {
            nonce: 1,
            balance: U256::from(1u64),
            bytecode_hash: None,
        }),
    );

    if let Some((key, value)) = slot {
        let mut storage = HashedStorage::new(false);
        storage
            .storage
            .insert(keccak256(key.to_be_bytes::<32>()), value);
        post_state.storages.insert(hashed_address, storage);
    }

    state.update(&post_state);
    state
}

/// A witness may only serve code under the hash of that code. Taking the map
/// key from the encoding would let a prover run any code it likes for a
/// deployed contract, since the state trie commits to the code hash and never
/// to the code behind it.
#[test]
fn test_partial_state_decode_keys_bytecode_by_its_own_hash() {
    let honest_code = Bytes::from_static(&[0x00]);
    let substituted_code = Bytes::from_static(&[0x60, 0x01, 0x00]);
    let honest_hash = keccak256(&honest_code);

    // A witness claiming the substituted code is what `honest_hash` commits to.
    let partial_state = EvmPartialState::new(
        single_account_state(Address::ZERO, None),
        BTreeMap::from([(honest_hash, Bytecode::new_raw(substituted_code.clone()))]),
        vec![],
    );

    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    assert!(
        decoded.bytecodes().get(&honest_hash).is_none(),
        "substituted code must not be reachable under the code hash it claimed"
    );
    assert_eq!(
        decoded
            .bytecodes()
            .get(&keccak256(&substituted_code))
            .map(|bytecode| bytecode.original_bytes()),
        Some(substituted_code),
        "code must be keyed by its own hash"
    );
}

/// `BLOCKHASH` returns these hashes, so they are computed from the headers
/// rather than carried alongside them.
#[test]
fn test_partial_state_decode_seals_headers_with_computed_hashes() {
    let headers = create_test_ancestor_headers();
    let partial_state = EvmPartialState::new(
        single_account_state(Address::ZERO, None),
        BTreeMap::new(),
        headers.clone(),
    );

    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    assert_block_hashes_match_headers(&decoded, &headers);
}

#[test]
fn test_partial_state_codec_roundtrip_with_storage() {
    let slot = (U256::from(3u64), U256::from(42u64));
    let partial_state = EvmPartialState::new(
        single_account_state(Address::ZERO, Some(slot)),
        BTreeMap::new(),
        vec![],
    );

    let encoded = encode_to_vec(&partial_state).expect("encode failed");
    let decoded: EvmPartialState = decode_buf_exact(&encoded).expect("decode failed");

    assert_eq!(
        decoded.ethereum_state().state_root(),
        partial_state.ethereum_state().state_root()
    );
}

/// The state root commits to each account's storage root, but nothing ties the
/// storage tries carried alongside to those roots. A trie holding values the
/// account never had must be rejected, or `SLOAD` would read them.
#[test]
fn test_partial_state_decode_rejects_mismatched_storage_root() {
    let address = Address::ZERO;
    let hashed_address = keccak256(address);

    let mut donor = single_account_state(address, Some((U256::from(3u64), U256::from(42u64))));
    let mut tampered = single_account_state(address, None);

    // Same account, but now carrying a storage trie its leaf never committed to.
    let forged_trie = donor
        .storage_tries
        .remove(&hashed_address)
        .expect("donor state must hold a storage trie");
    tampered.storage_tries.insert(hashed_address, forged_trie);

    let partial_state = EvmPartialState::new(tampered, BTreeMap::new(), vec![]);
    let encoded = encode_to_vec(&partial_state).expect("encode failed");

    let result = decode_buf_exact::<EvmPartialState>(&encoded);
    assert!(
        result.is_err(),
        "a storage trie that does not match the account's storage root must be rejected"
    );
}

/// Reads the storage root an account's leaf commits to.
fn account_storage_root(state: &EthereumState, hashed_address: B256) -> B256 {
    state
        .state_trie
        .get_rlp::<TrieAccount>(hashed_address.as_slice())
        .expect("account must be resolvable")
        .expect("account must be present in the state trie")
        .storage_root
}

/// Builds a write batch that only bumps an account's balance. Nothing here
/// reads or writes storage, which is what makes the wipe below silent.
fn balance_only_write_batch(hashed_address: B256, balance: u64) -> EvmWriteBatch {
    let mut post_state = HashedPostState::default();
    post_state.accounts.insert(
        hashed_address,
        Some(Account {
            nonce: 1,
            balance: U256::from(balance),
            bytecode_hash: None,
        }),
    );

    EvmWriteBatch::new(post_state)
}

/// A witness that omits an account's storage trie must not be allowed to
/// rewrite that account. `EthereumState::update` derives the new storage root
/// from the tries it holds, so merging without one would swap the account's
/// storage for an empty trie and erase every slot.
#[test]
fn test_merge_write_batch_rejects_missing_storage_trie() {
    let address = Address::ZERO;
    let hashed_address = keccak256(address);

    let mut state = single_account_state(address, Some((U256::from(3u64), U256::from(42u64))));
    assert_ne!(
        account_storage_root(&state, hashed_address),
        EMPTY_ROOT_HASH
    );

    // Drop the trie the account's leaf commits to, keeping the leaf itself.
    state
        .storage_tries
        .remove(&hashed_address)
        .expect("state must hold a storage trie");

    let mut partial_state = EvmPartialState::new(state, BTreeMap::new(), vec![]);
    let write_batch = balance_only_write_batch(hashed_address, 2);

    assert!(
        partial_state.merge_write_batch(&write_batch).is_err(),
        "rewriting an account without its storage trie must be rejected"
    );
}

/// The guard only rejects witnesses that cannot preserve storage. With the
/// trie present the same balance change goes through and the storage root
/// survives it.
#[test]
fn test_merge_write_batch_preserves_untouched_storage() {
    let address = Address::ZERO;
    let hashed_address = keccak256(address);

    let state = single_account_state(address, Some((U256::from(3u64), U256::from(42u64))));
    let storage_root = account_storage_root(&state, hashed_address);

    let mut partial_state = EvmPartialState::new(state, BTreeMap::new(), vec![]);
    let write_batch = balance_only_write_batch(hashed_address, 2);

    partial_state
        .merge_write_batch(&write_batch)
        .expect("merging a complete witness must succeed");

    assert_eq!(
        account_storage_root(partial_state.ethereum_state(), hashed_address),
        storage_root
    );
}
