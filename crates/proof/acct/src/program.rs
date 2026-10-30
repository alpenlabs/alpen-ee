use alpen_acct_runtime::ChunkInput;
use alpen_da_types::DaWitness;
use alpen_params::AlpenSpecId;
use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use ssz::Decode;
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_snark_acct_runtime::PrivateInput as UpdatePrivateInput;
use strata_snark_acct_types::UpdateProofPubParams;
use zkaleido::{
    ProofType, PublicValues, ZkVmEnvSerde, ZkVmError, ZkVmInputError, ZkVmInputResult, ZkVmProgram,
    ZkVmResult,
};
use zkaleido_native_adapter::NativeHost;

use crate::{process_ee_acct_update, v1, v2};

/// Host-side input for the EE account update proof, in the layout of the guest
/// released for the spec version it proves under.
///
/// The native host reads every layout. See [`process_ee_acct_update_native`].
///
/// Note: the chunk predicate key (VK of the chunk SP1 program) is not part of
/// this input. The acct guest receives it at compile time, baked into the ELF
/// by `provers/sp1/build.rs`. This is intentional — a host-supplied predicate
/// key would let a malicious prover bypass chunk proof verification. See
/// `provers/sp1/guest-alpen-acct/src/main.rs` for the guest-side construction
/// path.
///
/// For native testing, the predicate key lives on [`EeAcctProgram::new`] and
/// is passed into the `NativeHost` closure directly.
#[derive(Debug)]
pub enum EeAcctProofInput {
    /// The layout the V1 guest release reads. See [`v1`].
    V1 {
        ee_private_input: v1::EePrivateInput,
        snark_acct_private_input: UpdatePrivateInput,
        da_witness: v1::DaWitness,
    },

    /// The layout the V2 guests read. See [`v2`].
    V2 {
        ee_private_input: v2::EePrivateInput,
        /// Snark-account update private input (`snark_acct_runtime::PrivateInput`):
        /// the update pub-params, partial pre-state, and per-message coinputs.
        snark_acct_private_input: UpdatePrivateInput,
        /// Alpen-EE-specific witness input for verifying the batch's DA.
        da_witness: v2::DaWitness,
    },
}

impl EeAcctProofInput {
    /// Returns whether the guest released for `spec_version` finds the
    /// bytecode a DA blob omits through code holders in the pre-state.
    /// Otherwise it takes that bytecode's bytes.
    pub fn reads_code_holders(spec_version: AlpenSpecId) -> bool {
        match spec_version {
            AlpenSpecId::V0 | AlpenSpecId::V1 => false,
            AlpenSpecId::V2 => true,
        }
    }

    /// Builds the input in the layout the guest released for `spec_version`
    /// reads, keeping only the parts that layout carries.
    ///
    /// `raw_partial_pre_state` must hold the code holders exactly when
    /// [`Self::reads_code_holders`] is true. Otherwise the input carries
    /// `omitted_bytecodes` in place of `da_witness`'s holder keys.
    ///
    /// V0 shares V1's layout. No network proves V0 any more, so its own
    /// layout is not kept.
    pub fn new(
        spec_version: AlpenSpecId,
        raw_partial_pre_state: Vec<u8>,
        chunks: Vec<ChunkInput>,
        snark_acct_private_input: UpdatePrivateInput,
        da_witness: DaWitness,
        omitted_bytecodes: Vec<Vec<u8>>,
    ) -> Self {
        match spec_version {
            AlpenSpecId::V0 | AlpenSpecId::V1 => Self::V1 {
                ee_private_input: v1::EePrivateInput::new(raw_partial_pre_state, chunks),
                snark_acct_private_input,
                da_witness: v1::DaWitness::new(da_witness.blocks().to_vec(), omitted_bytecodes),
            },
            AlpenSpecId::V2 => Self::V2 {
                ee_private_input: v2::EePrivateInput::new(raw_partial_pre_state, chunks),
                snark_acct_private_input,
                da_witness,
            },
        }
    }
}

/// Runs the account check natively on an input built by
/// [`EeAcctProofInput::new`] for `spec_version`.
///
/// The native host runs this source's check for every version. It reads each
/// version's layout, so both backends take the same input.
pub fn process_ee_acct_update_native(
    zkvm: &impl ZkVmEnvSerde,
    spec_version: AlpenSpecId,
    chunk_predicate_key: &PredicateKey,
) {
    match spec_version {
        AlpenSpecId::V0 | AlpenSpecId::V1 => {
            v1::process_ee_acct_update(zkvm, spec_version, chunk_predicate_key)
        }
        AlpenSpecId::V2 => process_ee_acct_update(zkvm, spec_version, chunk_predicate_key),
    }
}

#[derive(Debug)]
pub struct EeAcctProgram {
    chunk_predicate_key: PredicateKey,
    spec_version: AlpenSpecId,
}

impl EeAcctProgram {
    pub fn new(chunk_predicate_key: PredicateKey, spec_version: AlpenSpecId) -> Self {
        Self {
            chunk_predicate_key,
            spec_version,
        }
    }
}

impl ZkVmProgram for EeAcctProgram {
    type Input = EeAcctProofInput;
    type Output = UpdateProofPubParams;

    fn name() -> String {
        "EVM EE Account".to_string()
    }

    fn proof_type() -> ProofType {
        ProofType::Groth16
    }

    fn prepare_input<'a, B>(input: &'a Self::Input) -> ZkVmInputResult<B::Input>
    where
        B: zkaleido::ZkVmInputBuilder<'a>,
    {
        let (ee_rkyv_bytes, upd_rkyv_bytes, da_rkyv_bytes) = match input {
            EeAcctProofInput::V1 {
                ee_private_input,
                snark_acct_private_input,
                da_witness,
            } => (
                rkyv::to_bytes::<RkyvError>(ee_private_input),
                rkyv::to_bytes::<RkyvError>(snark_acct_private_input),
                rkyv::to_bytes::<RkyvError>(da_witness),
            ),
            EeAcctProofInput::V2 {
                ee_private_input,
                snark_acct_private_input,
                da_witness,
            } => (
                rkyv::to_bytes::<RkyvError>(ee_private_input),
                rkyv::to_bytes::<RkyvError>(snark_acct_private_input),
                rkyv::to_bytes::<RkyvError>(da_witness),
            ),
        };

        let mut builder = B::new();
        for bytes in [ee_rkyv_bytes, upd_rkyv_bytes, da_rkyv_bytes] {
            let bytes = bytes.map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
            builder.write_buf(&bytes)?;
        }
        builder.build()
    }

    fn process_output<H>(public_values: &PublicValues) -> ZkVmResult<Self::Output>
    where
        H: zkaleido::ZkVmHost,
    {
        UpdateProofPubParams::from_ssz_bytes(public_values.as_bytes())
            .map_err(|e| ZkVmError::Other(e.to_string()))
    }
}

impl EeAcctProgram {
    /// Deterministic Schnorr signing key backing [`Self::test_predicate_key`] and
    /// [`Self::native_host`].
    pub fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x02u8; 32]).expect("valid test signing key")
    }

    /// Native host that can be used for testing.
    pub fn native_host(&self) -> NativeHost {
        let key = self.chunk_predicate_key.clone();
        let spec_version = self.spec_version;
        NativeHost::new(Self::test_signing_key(), move |zkvm| {
            process_ee_acct_update_native(zkvm, spec_version, &key)
        })
    }

    /// Predicate key matching [`Self::test_signing_key`], for wiring into functional-test
    /// params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = Self::test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, pk)
            .expect("verifying key fits within the condition length limit")
    }

    /// Executes the account proof program using the native host, for testing.
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
    use alpen_acct_types::{EeAccountState, UpdateExtraData};
    use alpen_da_types::DaWitness;
    use alpen_params::{AlpenParams, DEV_PARAMS_JSON};
    use alpen_proof_chunk::{EeChunkProgram, test_utils::build_workload_chunk};
    use alpen_test_utils_evm_workload::{MIXED_WORKLOAD, Workload, workload_dir};
    use ssz::Encode;
    use strata_codec::encode_to_vec;
    use strata_identifiers::Hash;
    use strata_predicate::{PredicateKey, PredicateTypeId};
    use strata_snark_acct_runtime::{IInnerState, PrivateInput as UpdatePrivateInput};
    use strata_snark_acct_types::{
        LedgerRefs, ProofState, Seqno, UpdateOutputs, UpdateProofPubParams,
    };

    use super::*;
    use crate::test_utils::build_workload_update;

    /// Smoke test: constructs a minimal self-consistent input with zero chunks
    /// and zero messages, and runs it through the full native execution
    /// pipeline in each version's layout.
    #[test]
    fn test_native_acct_execution_zero_chunks() {
        for spec_version in [AlpenSpecId::V1, AlpenSpecId::V2] {
            run_native_acct_execution_zero_chunks(spec_version);
        }
    }

    fn run_native_acct_execution_zero_chunks(spec_version: AlpenSpecId) {
        // Build a minimal EE account state.
        let initial_blkid = Hash::zero();
        let initial_state =
            EeAccountState::new(initial_blkid, Hash::zero(), Vec::new(), Vec::new());
        let state_root = initial_state.compute_state_root();

        // Extra data: tip stays the same, nothing processed.
        let extra_data =
            UpdateExtraData::new(initial_blkid, initial_state.last_exec_state_root(), 0, 0);
        let extra_data_bytes = encode_to_vec(&extra_data).expect("encode extra data");

        // With zero chunks and no state change, pre == post state root.
        let pub_params = UpdateProofPubParams::new(
            Seqno::zero(),
            ProofState::new(state_root, 0),
            ProofState::new(state_root, 0),
            vec![],
            LedgerRefs::new_empty(),
            UpdateOutputs::new_empty(),
            extra_data_bytes,
        );

        // Construct private inputs.
        let snark_acct_private_input =
            UpdatePrivateInput::new(pub_params, initial_state.as_ssz_bytes(), vec![]);
        let proof_input = EeAcctProofInput::new(
            spec_version,
            vec![],
            vec![],
            snark_acct_private_input,
            DaWitness::empty(),
            vec![],
        );

        // Predicate is carried through but never evaluated in this
        // zero-chunks test; either `always_accept` or a real Schnorr
        // key would work. Using `Bip340Schnorr` to exercise the
        // non-trivial path.
        let program = EeAcctProgram::new(
            PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![0u8; 32])
                .expect("condition fits within the length limit"),
            spec_version,
        );
        let result = program
            .execute(&proof_input)
            .expect("native execution should succeed");

        // Verify output pub params state roots match.
        assert_eq!(result.cur_state().inner_state(), state_root);
        assert_eq!(result.new_state().inner_state(), state_root);
    }

    /// Proves the mixed workload's chunk natively, then the account update
    /// that carries it, so the update runs real deposits, outputs and DA.
    #[test]
    fn test_native_acct_execution_on_workload() {
        let workload = Workload::load(&workload_dir(MIXED_WORKLOAD)).expect("load workload");
        let params: AlpenParams =
            serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse");
        let chunk = build_workload_chunk(&workload, &params, AlpenSpecId::V2);

        // Natively the chunk proof is a Schnorr signature over the transition,
        // checked against the chunk program's test key.
        let chunk_program = EeChunkProgram::new(params, AlpenSpecId::V2);
        let chunk_proof =
            <EeChunkProgram as ZkVmProgram>::prove(&chunk.input, &chunk_program.native_host())
                .expect("native chunk proof");
        let input = build_workload_update(
            &workload,
            &chunk.transition,
            chunk_proof.receipt().proof().as_bytes().to_vec(),
            AlpenSpecId::V2,
        );

        let output = EeAcctProgram::new(EeChunkProgram::test_predicate_key(), AlpenSpecId::V2)
            .execute(&input)
            .expect("native execution should succeed");

        assert_eq!(
            output.new_state().next_inbox_msg_idx(),
            chunk.transition.inputs().subject_deposits().len() as u64
        );
        assert!(!output.outputs().messages().is_empty());
    }
}
