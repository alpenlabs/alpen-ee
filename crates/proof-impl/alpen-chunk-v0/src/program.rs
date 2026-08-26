use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use rsp_primitives::genesis::Genesis;
use ssz::{Decode, Encode};
use strata_bridge_params::BridgeParams;
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
    pub genesis: Genesis,
    pub private_input: PrivateInput,
    pub bridge_params: BridgeParams,
}

#[derive(Debug)]
pub struct EeChunkProgram;

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
        builder.write_serde(&input.genesis)?;
        let rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&rkyv_bytes)?;
        builder.write_buf(&input.bridge_params.as_ssz_bytes())?;
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
    pub fn native_host() -> NativeHost {
        NativeHost::new(test_signing_key(), process_ee_chunk)
    }

    /// Predicate key matching the signing key the native host uses, for wiring into
    /// functional-test params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, pk)
            .expect("verifying key fits within the condition length limit")
    }

    /// Executes the chunk proof program using the native host for testing.
    pub fn execute(
        input: &<Self as ZkVmProgram>::Input,
    ) -> ZkVmResult<<Self as ZkVmProgram>::Output> {
        let host = Self::native_host();
        let summary = <Self as ZkVmProgram>::execute(input, &host)?;
        <Self as ZkVmProgram>::process_output::<NativeHost>(summary.public_values())
    }
}
