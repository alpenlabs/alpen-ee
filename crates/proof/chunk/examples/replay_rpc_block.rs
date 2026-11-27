//! Replays a single V2 block from independently downloaded header, RLP and witness.
#![expect(
    unused_crate_dependencies,
    reason = "the standalone replay uses a subset of the proof crate dependencies"
)]
use std::{env, fs, path::Path, str::from_utf8};

use alloy_consensus::Header;
use alloy_rlp::decode_exact;
use alpen_acct_types::{ExecBlock, ExecHeader, ExecPartialState, ExecutionEnvironment};
use alpen_chain_types::{ChunkTransition, ExecInputs};
use alpen_chunk_runtime::RawBlockData;
use alpen_evm_ee::{EvmBlock, EvmExecutionEnvironment, EvmHeader, EvmPartialState};
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_proof_chunk::{EeChunkProgram, EeChunkProofInput};
use reth_ethereum_primitives::Block;
use rkyv::rancor::Error as RkyvError;
use serde_json::Value;
use ssz::Encode;
use strata_codec::encode_to_vec;

fn from_hex(s: &str) -> Vec<u8> {
    let (pairs, tail) = s.strip_prefix("0x").unwrap().as_bytes().as_chunks::<2>();
    assert!(
        tail.is_empty(),
        "hex input must have an even number of digits"
    );
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    format!(
        "0x{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn rpc(dir: &Path, name: &str) -> Value {
    serde_json::from_slice::<Value>(&fs::read(dir.join(format!("{name}.json"))).unwrap())
        .unwrap()["result"].clone()
}

fn main() {
    let mut argv = env::args().skip(1);
    let path = argv.next().expect("fixture directory");
    let dir = Path::new(&path);
    let height = argv.next().expect("fixture block number");
    let genesis = rpc(dir, "staging-genesis");
    let block_rpc = rpc(dir, &format!("staging-{height}-block"));
    let parent_rpc = rpc(dir, &format!("staging-{height}-parent"));
    let raw_block = from_hex(rpc(dir, &format!("staging-{height}-raw")).as_str().unwrap());
    let witness = rpc(dir, &format!("staging-{height}-witness"));
    let params: AlpenParams =
        serde_json::from_slice(&fs::read(dir.join("staging-alpen-params.json")).unwrap()).unwrap();
    assert_eq!(
        to_hex(&params.genesis_block_info().blockhash().0),
        genesis["hash"].as_str().unwrap()
    );
    let bytes_list = |name: &str| -> Vec<Vec<u8>> {
        witness[name]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| from_hex(value.as_str().unwrap()))
            .collect()
    };
    let mut headers: Vec<Header> = bytes_list("headers")
        .iter()
        .map(|data| decode_exact(&data[..]).unwrap())
        .collect();
    headers.sort_unstable_by_key(|header| header.number);
    let parent = EvmHeader::new(headers.last().unwrap().clone());
    let parent_hash = to_hex(&parent.compute_block_id().0);
    assert_eq!(parent_hash, parent_rpc["hash"].as_str().unwrap());
    assert_eq!(parent_hash, block_rpc["parentHash"].as_str().unwrap());
    assert_eq!(
        to_hex(&parent.get_state_root().0),
        parent_rpc["stateRoot"].as_str().unwrap()
    );

    let block: Block = decode_exact(&raw_block[..]).unwrap();
    assert!(
        block
            .body
            .withdrawals
            .as_ref()
            .is_none_or(|ws| ws.is_empty())
    );
    let block = EvmBlock::new(block);
    let expected = block.get_header().clone();
    let expected_hash = block_rpc["hash"].as_str().unwrap();
    let expected_root = block_rpc["stateRoot"].as_str().unwrap();
    assert_eq!(to_hex(&expected.compute_block_id().0), expected_hash);
    assert_eq!(to_hex(&expected.get_state_root().0), expected_root);
    if let Some(independent) = argv.next() {
        assert_eq!(expected_hash, independent, "block explorer and RPC agree");
    }
    let mut state = EvmPartialState::from_witness_parts(
        bytes_list("state"),
        parent.get_state_root().0.into(),
        bytes_list("codes"),
        headers,
    );
    let state_before = encode_to_vec(&state).unwrap();
    let inputs = ExecInputs::new_empty();
    let env = EvmExecutionEnvironment::new(&params, AlpenSpecId::V2);
    env.verify_header(&parent, &expected).unwrap();
    env.verify_block_pre_execution(&block, &inputs).unwrap();
    let out = env.execute_block(&state, block.clone()).unwrap();
    let outputs = out.outputs().clone();
    env.verify_block_post_execution(&expected, &out, &outputs)
        .unwrap();
    env.merge_write_into_state(&mut state, out.write_batch())
        .unwrap();
    let actual_root = state.compute_state_root();
    println!(
        "block={height} hash={expected_hash} expected_root={expected_root} actual_root={}",
        to_hex(&actual_root.0)
    );
    assert_eq!(to_hex(&actual_root.0), expected_root);

    let transition = ChunkTransition::new(
        parent.compute_block_id(),
        expected.compute_block_id(),
        actual_root,
        expected.get_exec_header_summary(),
        inputs.clone(),
        outputs.clone(),
    );
    let input = EeChunkProofInput::new(
        AlpenSpecId::V2,
        transition,
        vec![RawBlockData::from_block::<EvmExecutionEnvironment>(&block, inputs, outputs).unwrap()],
        encode_to_vec(&parent).unwrap(),
        state_before,
    );

    let executed = EeChunkProgram::new(params, AlpenSpecId::V2)
        .execute(&input)
        .unwrap();
    assert_eq!(to_hex(&executed.tip_exec_blkid().0), expected_hash);
    assert_eq!(to_hex(&executed.tip_state_root().0), expected_root);
    println!("native_chunk_guest_verified=true");
    fs::write(
        dir.join(format!("staging-{height}-chunk-public.ssz")),
        executed.as_ssz_bytes(),
    )
    .unwrap();

    if let EeChunkProofInput::V2 { private_input } = input {
        let archive = rkyv::to_bytes::<RkyvError>(&private_input).unwrap();
        fs::write(
            dir.join(format!("staging-{height}-chunk-input.rkyv")),
            &archive,
        )
        .unwrap();
    }
}
