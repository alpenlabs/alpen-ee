use alpen_chain_types::ChunkTransition;
use alpen_chunk_runtime::PrivateInput;
use alpen_params::{AlpenParams, AlpenSpecId};
use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use ssz::Decode;
use strata_predicate::{PredicateKey, PredicateTypeId};
use zkaleido::{
    ProofType, PublicValues, ZkVmError, ZkVmInputError, ZkVmInputResult, ZkVmProgram, ZkVmResult,
};
use zkaleido_native_adapter::NativeHost;

use crate::process_ee_chunk;

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
    spec_version: AlpenSpecId,
}

impl EeChunkProgram {
    pub fn new(params: AlpenParams, spec_version: AlpenSpecId) -> Self {
        Self {
            params,
            spec_version,
        }
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
    /// Deterministic Schnorr signing key backing [`Self::test_predicate_key`] and
    /// [`Self::native_host`].
    pub fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x03u8; 32]).expect("valid test signing key")
    }

    /// Native host that can be used for testing.
    pub fn native_host(&self) -> NativeHost {
        let params = self.params.clone();
        let spec_version = self.spec_version;
        NativeHost::new(Self::test_signing_key(), move |zkvm| {
            process_ee_chunk(zkvm, &params, spec_version)
        })
    }

    /// Predicate key matching [`Self::test_signing_key`], for wiring into functional-test
    /// params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = Self::test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, pk)
            .expect("verifying key fits within the condition length limit")
    }

    /// Executes the chunk proof program using the native host, for testing.
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
    use alpen_params::DEV_PARAMS_JSON;
    use alpen_test_utils_evm_workload::{MIXED_WORKLOAD, Workload, workload_dir};

    use super::*;
    use crate::test_utils::build_workload_chunk;

    #[test]
    fn test_native_chunk_execution() {
        let workload = Workload::load(&workload_dir(MIXED_WORKLOAD)).expect("load workload");
        let params: AlpenParams =
            serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse");
        let chunk = build_workload_chunk(&workload, &params, AlpenSpecId::V1);

        let output = EeChunkProgram::new(params, AlpenSpecId::V1)
            .execute(&chunk.input)
            .expect("native execution should succeed");

        assert_eq!(output, chunk.transition);
        assert_eq!(
            output.inputs().subject_deposits().len(),
            workload
                .blocks
                .iter()
                .map(|block| block.deposits.len())
                .sum::<usize>()
        );
        assert!(!output.outputs().output_messages().is_empty());
    }
}
