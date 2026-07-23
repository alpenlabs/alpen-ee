use alpen_ee_da_types::DaWitness;
use k256::schnorr::SigningKey;
use rkyv::rancor::Error as RkyvError;
use rsp_primitives::genesis::Genesis;
use ssz::{Decode, Encode};
use strata_bridge_params::BridgeParams;
use strata_ee_acct_runtime::EePrivateInput;
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_snark_acct_runtime::PrivateInput as UpdatePrivateInput;
use strata_snark_acct_types::UpdateProofPubParams;
use zkaleido::{
    ProofType, PublicValues, ZkVmError, ZkVmInputError, ZkVmInputResult, ZkVmProgram, ZkVmResult,
};
use zkaleido_native_adapter::NativeHost;

use crate::process_ee_acct_update;

fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[0x02u8; 32]).expect("valid test signing key")
}

/// Second deterministic test signing key, standing in for the "new ELF" of a
/// live VK rotation in native-prover tests.
fn test_signing_key_v2() -> SigningKey {
    SigningKey::from_bytes(&[0x04u8; 32]).expect("valid test signing key")
}

/// Host-side input for the EE account update proof.
///
/// Note: the chunk predicate key (VK of the chunk SP1 program) is NOT
/// part of this input. The acct guest receives it at compile time via
/// `vks::GUEST_ALPEN_CHUNK_VK_CONDITION`, baked by `provers/sp1/build.rs`
/// from the chunk program's Groth16 VK. This is intentional — a
/// host-supplied key would let a malicious prover bypass chunk proof
/// verification. See `provers/sp1/guest-alpen-acct/src/main.rs` for the
/// guest-side construction path.
///
/// For native testing, the key lives on [`EeAcctProgram::new`] and is
/// passed into the `NativeHost` closure directly.
#[derive(Debug)]
pub struct EeAcctProofInput {
    pub genesis: Genesis,
    pub ee_private_input: EePrivateInput,
    /// Snark-account update private input (`snark_acct_runtime::PrivateInput`):
    /// the update pub-params, partial pre-state, and per-message coinputs.
    pub snark_acct_private_input: UpdatePrivateInput,
    /// Alpen-EE-specific witness input for verifying the batch's DA.
    pub da_witness: DaWitness,
    /// Bridge withdrawal denomination and cap, parameterizing the EVM.
    pub bridge_params: BridgeParams,
    /// The VK the batch this proof covers was stamped with at seal time.
    ///
    /// Host-side routing metadata only: a version-aware prove strategy uses
    /// it to pick the host matching the batch's VK across a live rotation.
    /// It is deliberately NOT written into the guest input (see
    /// `prepare_input`) — the verifier's key choice is authoritative on OL,
    /// not attested by the proof itself.
    pub update_vk: PredicateKey,
}

#[derive(Debug)]
pub struct EeAcctProgram {
    chunk_predicate_key: PredicateKey,
}

impl EeAcctProgram {
    pub fn new(chunk_predicate_key: PredicateKey) -> Self {
        Self {
            chunk_predicate_key,
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
        builder.write_serde(&input.genesis)?;

        let ee_rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.ee_private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&ee_rkyv_bytes)?;

        let upd_rkyv_bytes = rkyv::to_bytes::<RkyvError>(&input.snark_acct_private_input)
            .map_err(|e| ZkVmInputError::InputBuild(e.to_string()))?;
        builder.write_buf(&upd_rkyv_bytes)?;
        builder.write_buf(&input.bridge_params.as_ssz_bytes())?;

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
    pub fn native_host(&self) -> NativeHost {
        self.native_host_with_key(test_signing_key())
    }

    /// Native host signing under the v2 test key — the "new ELF" of a live
    /// VK rotation in native-prover tests.
    pub fn native_host_v2(&self) -> NativeHost {
        self.native_host_with_key(test_signing_key_v2())
    }

    fn native_host_with_key(&self, signing_key: SigningKey) -> NativeHost {
        let key = self.chunk_predicate_key.clone();
        NativeHost::new(signing_key, move |zkvm| process_ee_acct_update(zkvm, &key))
    }

    /// Predicate key matching the signing key the native host uses, for wiring into
    /// functional-test params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::new(PredicateTypeId::Bip340Schnorr, pk)
    }

    /// Predicate key matching the v2 native host's signing key.
    pub fn test_predicate_key_v2() -> PredicateKey {
        let pk = test_signing_key_v2().verifying_key().to_bytes().to_vec();
        PredicateKey::new(PredicateTypeId::Bip340Schnorr, pk)
    }

    /// Executes the account proof program using the native host for testing.
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
    use rsp_primitives::genesis::Genesis;
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

        // Use Mainnet genesis (valid ChainSpec, not used with zero chunks).
        let genesis = Genesis::Mainnet;

        let proof_input = EeAcctProofInput {
            update_vk: EeAcctProgram::test_predicate_key(),
            genesis,
            ee_private_input,
            snark_acct_private_input,
            da_witness: DaWitness::empty(),
            bridge_params: BridgeParams::default(),
        };

        // Predicate is carried through but never evaluated in this
        // zero-chunks test; either `always_accept` or a real Schnorr
        // key would work. Using `Bip340Schnorr` to exercise the
        // non-trivial path.
        let program = EeAcctProgram::new(PredicateKey::new(
            PredicateTypeId::Bip340Schnorr,
            vec![0u8; 32],
        ));
        let result = program
            .execute(&proof_input)
            .expect("native execution should succeed");

        // Verify output pub params state roots match.
        assert_eq!(result.cur_state().inner_state(), state_root);
        assert_eq!(result.new_state().inner_state(), state_root);
    }
}

#[cfg(test)]
mod v2_key_tests {
    use super::EeAcctProgram;

    /// The v2 test predicate in CLI string form, for functional tests that
    /// rotate the account to the native prover's second key. Keep in sync
    /// with `V2_ACCT_PREDICATE` in
    /// `functional-tests/tests/alpen_client/test_ee_live_fork_upgrade.py`.
    const V2_PREDICATE_STR: &str =
        "Bip340Schnorr:462779ad4aad39514614751a71085f2f10e1c7a593e4e030efb5b8721ce55b0b";

    #[test]
    fn v2_predicate_key_matches_pinned_string() {
        let key = EeAcctProgram::test_predicate_key_v2();
        let encoded = serde_json::to_string(&key).expect("predicate key serializes");
        assert_eq!(encoded.trim_matches('"'), V2_PREDICATE_STR);
    }
}
