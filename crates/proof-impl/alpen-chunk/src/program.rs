use alpen_ee_params::AlpenParams;
use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use ssz::Decode;
use strata_ee_chain_types::ChunkTransition;
use strata_ee_chunk_runtime::PrivateInput;
use strata_predicate::{PredicateKey, PredicateTypeId};
use zkaleido::{
    ProofType, PublicValues, ZkVmError, ZkVmInputError, ZkVmInputResult, ZkVmProgram, ZkVmResult,
};
use zkaleido_native_adapter::NativeHost;

use crate::process_ee_chunk;

fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[0x03u8; 32]).expect("valid test signing key")
}

/// Host-side input for the EE chunk proof.
#[derive(Debug)]
pub struct EeChunkProofInput {
    pub private_input: PrivateInput,
}

/// Note: `AlpenParams` (genesis, bridge params) is NOT part of this input.
/// The chunk guest receives it at compile time, baked into the ELF by
/// `provers/sp1/build.rs`. This is intentional — see [`crate::process_ee_chunk`].
///
/// For native testing, `params` lives on [`EeChunkProgram::new`] and is
/// passed into the `NativeHost` closure directly.
#[derive(Debug)]
pub struct EeChunkProgram {
    params: AlpenParams,
}

impl EeChunkProgram {
    pub fn new(params: AlpenParams) -> Self {
        Self { params }
    }
}

impl ZkVmProgram for EeChunkProgram {
    type Input = EeChunkProofInput;
    type Output = ChunkTransition;

    fn name() -> String {
        "EVM EE Chunk".to_string()
    }

    fn proof_type() -> ProofType {
        ProofType::Groth16
    }

    fn prepare_input<'a, B>(input: &'a Self::Input) -> ZkVmInputResult<B::Input>
    where
        B: zkaleido::ZkVmInputBuilder<'a>,
    {
        let mut builder = B::new();
        let rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&rkyv_bytes)?;
        builder.build()
    }

    fn process_output<H>(public_values: &PublicValues) -> ZkVmResult<Self::Output>
    where
        H: zkaleido::ZkVmHost,
    {
        ChunkTransition::from_ssz_bytes(public_values.as_bytes())
            .map_err(|e| ZkVmError::Other(e.to_string()))
    }
}

impl EeChunkProgram {
    pub fn native_host(&self) -> NativeHost {
        let params = self.params.clone();
        NativeHost::new(test_signing_key(), move |zkvm| {
            process_ee_chunk(zkvm, &params)
        })
    }

    /// Predicate key matching the signing key the native host uses, for wiring into
    /// functional-test params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::new(PredicateTypeId::Bip340Schnorr, pk)
    }

    /// Executes the chunk proof program using the native host for testing.
    pub fn execute(
        &self,
        input: &<Self as ZkVmProgram>::Input,
    ) -> ZkVmResult<<Self as ZkVmProgram>::Output> {
        let host = self.native_host();
        let summary = <Self as ZkVmProgram>::execute(input, &host)?;
        <Self as ZkVmProgram>::process_output::<NativeHost>(summary.public_values())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use alpen_ee_params::{AlpenSpecSchedule, BlobSpec, DEFAULT_ALPEN_EE_ACCOUNT_ID, EvmSpec};
    use alpen_reth_evm::evm::AlpenEvmFactory;
    use reth_primitives_traits::Block as _;
    use rsp_client_executor::io::EthClientExecutorInput;
    use serde::Deserialize;
    use strata_acct_types::Hash;
    use strata_bridge_params::BridgeParams;
    use strata_codec::encode_to_vec;
    use strata_ee_acct_types::{ExecBlock, ExecHeader, ExecPayload, ExecutionEnvironment};
    use strata_ee_chain_types::{ChunkTransition, ExecInputs};
    use strata_ee_chunk_runtime::{PrivateInput, RawBlockData, RawChunkData};
    use strata_evm_ee::{
        EvmBlock, EvmBlockBody, EvmExecutionEnvironment, EvmHeader, EvmPartialState,
    };
    use strata_l1_txfmt::MagicBytes;

    use super::*;

    #[derive(Deserialize)]
    struct WitnessData {
        witness: EthClientExecutorInput,
    }

    fn load_witness() -> EthClientExecutorInput {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-utils/data/evm_ee/witness_params.json");
        let json = fs::read_to_string(path).expect("read witness JSON");
        let data: WitnessData = serde_json::from_str(&json).expect("parse witness JSON");
        data.witness
    }

    /// The dev-network `AlpenParams` this test exercises `process_ee_chunk`
    /// against — chosen because `witness_params.json`'s embedded genesis
    /// (chain id 2892, all hardforks active from genesis) matches it.
    fn dev_alpen_params() -> AlpenParams {
        let evm_spec: EvmSpec =
            serde_json::from_str(alpen_chainspec::DEV_CHAIN_SPEC).expect("dev chain should parse");
        AlpenParams::new(
            DEFAULT_ALPEN_EE_ACCOUNT_ID,
            BridgeParams::default(),
            BlobSpec::new(MagicBytes::new(*b"ALPN")),
            AlpenSpecSchedule::genesis(),
            evm_spec,
        )
    }

    #[test]
    fn test_native_chunk_execution() {
        let witness = load_witness();
        let params = dev_alpen_params();

        // Extract parent header (last ancestor = direct parent of current block).
        let parent_header = witness
            .ancestor_headers
            .last()
            .expect("need at least one ancestor header")
            .clone();
        let parent_evm_header = EvmHeader::new(parent_header);
        let parent_blkid: Hash = parent_evm_header.compute_block_id();

        // Build partial pre-state from witness data.
        let pre_state = EvmPartialState::new(
            witness.parent_state,
            // This RSP fixture stores bytecodes as a Vec without original code-hash
            // keys. Re-hashing keeps the fixture behavior; production range
            // witnesses preserve the AccessedStateGenerator keys instead.
            witness
                .bytecodes
                .into_iter()
                .map(|bytecode| (bytecode.hash_slow(), bytecode))
                .collect(),
            witness.ancestor_headers,
        );

        // Build the EVM block from the witness.
        let header = witness.current_block.header().clone();
        let evm_header = EvmHeader::new(header.clone());
        let body = EvmBlockBody::from_alloy_body(witness.current_block.body().clone());
        let block = EvmBlock::new(evm_header, body);
        let tip_blkid: Hash = block.get_header().compute_block_id();
        let tip_state_root = block.get_header().get_state_root();
        let tip_exec_header_summary = block.get_header().get_exec_header_summary();

        // Execute the block to get outputs, against the same params `params`
        // will hand to `process_ee_chunk` below.
        let chain_spec: Arc<reth_chainspec::ChainSpec> =
            Arc::new(params.evm_spec().chain_spec().clone());
        let ee = EvmExecutionEnvironment::new(chain_spec, AlpenEvmFactory::default());
        let header_intrinsics = block.get_header().get_intrinsics();
        let exec_payload = ExecPayload::new(&header_intrinsics, block.get_body());
        let inputs = ExecInputs::new_empty();
        let output = ee
            .execute_block_body(&pre_state, &exec_payload, &inputs)
            .expect("block execution should succeed");
        let outputs = output.outputs().clone();

        // Build chunk transition.
        let chunk_transition = ChunkTransition::new(
            parent_blkid,
            tip_blkid,
            tip_state_root,
            tip_exec_header_summary,
            inputs.clone(),
            outputs.clone(),
        );

        // Single-block chunk: the chunk-level pre-state is just this block's
        // pre-state, anchored at the parent root.
        let raw_chunk_pre_state = encode_to_vec(&pre_state).expect("encode pre-state");
        let raw_block_data =
            RawBlockData::from_block::<EvmExecutionEnvironment>(&block, inputs, outputs)
                .expect("encode block");
        let raw_chunk = RawChunkData::new(vec![raw_block_data], parent_blkid);
        let raw_prev_header = encode_to_vec(&parent_evm_header).expect("encode prev header");

        let private_input = PrivateInput::new(
            chunk_transition.clone(),
            raw_chunk,
            raw_prev_header,
            raw_chunk_pre_state,
        );

        let proof_input = EeChunkProofInput { private_input };

        // Run the full native execution pipeline.
        let result = EeChunkProgram::new(params)
            .execute(&proof_input)
            .expect("native execution should succeed");

        assert_eq!(result.parent_exec_blkid(), parent_blkid);
        assert_eq!(result.tip_exec_blkid(), tip_blkid);
    }
}
