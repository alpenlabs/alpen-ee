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
        let key = self.chunk_predicate_key.clone();
        NativeHost::new(test_signing_key(), move |zkvm| {
            process_ee_acct_update(zkvm, &key)
        })
    }

    /// Predicate key matching the signing key the native host uses, for wiring into
    /// functional-test params so the resulting witness verifies under `Bip340Schnorr`.
    pub fn test_predicate_key() -> PredicateKey {
        let pk = test_signing_key().verifying_key().to_bytes().to_vec();
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, pk)
            .expect("verifying key fits within the condition length limit")
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
