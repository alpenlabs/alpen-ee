use alpen_ee_da_types::DaWitness;
use alpen_ee_params::{AlpenParams, AlpenSpecId};
use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use ssz::Decode;
use strata_ee_acct_runtime::EePrivateInput;
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_snark_acct_runtime::PrivateInput as UpdatePrivateInput;
use strata_snark_acct_types::UpdateProofPubParams;
use zkaleido::{
    ProofType, PublicValues, ZkVmError, ZkVmInputError, ZkVmInputResult, ZkVmProgram, ZkVmResult,
};
use zkaleido_native_adapter::NativeHost;

use crate::process_ee_acct_update;

/// Host-side input for the EE account update proof.
///
/// Note: neither `AlpenParams` (genesis, bridge params) nor the chunk
/// predicate key (VK of the chunk SP1 program) is part of this input. The
/// acct guest receives both at compile time, baked into the ELF by
/// `provers/sp1/build.rs`. This is intentional — a host-supplied genesis,
/// bridge params, or predicate key would let a malicious prover bypass
/// consensus-critical checks or chunk proof verification. See
/// `provers/sp1/guest-alpen-acct/src/main.rs` for the guest-side
/// construction path.
///
/// For native testing, `params` and the predicate key live on
/// [`EeAcctProgram::new`] and are passed into the `NativeHost` closure
/// directly.
#[derive(Debug)]
pub struct EeAcctProofInput {
    pub ee_private_input: EePrivateInput,
    /// Snark-account update private input (`snark_acct_runtime::PrivateInput`):
    /// the update pub-params, partial pre-state, and per-message coinputs.
    pub snark_acct_private_input: UpdatePrivateInput,
    /// Alpen-EE-specific witness input for verifying the batch's DA.
    pub da_witness: DaWitness,
}

#[derive(Debug)]
pub struct EeAcctProgram {
    chunk_predicate_key: PredicateKey,
    params: AlpenParams,
    spec_version: AlpenSpecId,
}

impl EeAcctProgram {
    pub fn new(
        chunk_predicate_key: PredicateKey,
        params: AlpenParams,
        spec_version: AlpenSpecId,
    ) -> Self {
        Self {
            chunk_predicate_key,
            params,
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
        let mut builder = B::new();

        let ee_rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.ee_private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&ee_rkyv_bytes)?;

        let upd_rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.snark_acct_private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&upd_rkyv_bytes)?;

        let da_rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.da_witness)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&da_rkyv_bytes)?;

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
        let params = self.params.clone();
        let spec_version = self.spec_version;
        NativeHost::new(Self::test_signing_key(), move |zkvm| {
            process_ee_acct_update(zkvm, &params, spec_version, &key)
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
    use alpen_ee_da_types::DaWitness;
    use ssz::Encode;
    use strata_codec::encode_to_vec;
    use strata_ee_acct_runtime::EePrivateInput;
    use strata_ee_acct_types::{EeAccountState, UpdateExtraData};
    use strata_identifiers::Hash;
    use strata_predicate::{PredicateKey, PredicateTypeId};
    use strata_snark_acct_runtime::{IInnerState, PrivateInput as UpdatePrivateInput};
    use strata_snark_acct_types::{
        LedgerRefs, ProofState, Seqno, UpdateOutputs, UpdateProofPubParams,
    };

    use super::*;

    /// Smoke test: constructs a minimal self-consistent input with zero chunks
    /// and zero messages, and runs through the full native execution pipeline.
    #[test]
    fn test_native_acct_execution_zero_chunks() {
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
        let ee_private_input = EePrivateInput::new(vec![], vec![], vec![]);

        // Not exercised with zero chunks, so the default (empty EVM genesis)
        // params are fine here.
        let params = AlpenParams::default();

        let proof_input = EeAcctProofInput {
            ee_private_input,
            snark_acct_private_input,
            da_witness: DaWitness::empty(),
        };

        // Predicate is carried through but never evaluated in this
        // zero-chunks test; either `always_accept` or a real Schnorr
        // key would work. Using `Bip340Schnorr` to exercise the
        // non-trivial path.
        let program = EeAcctProgram::new(
            PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![0u8; 32])
                .expect("condition fits within the length limit"),
            params,
            AlpenSpecId::V0,
        );
        let result = program
            .execute(&proof_input)
            .expect("native execution should succeed");

        // Verify output pub params state roots match.
        assert_eq!(result.cur_state().inner_state(), state_root);
        assert_eq!(result.new_state().inner_state(), state_root);
    }
}
