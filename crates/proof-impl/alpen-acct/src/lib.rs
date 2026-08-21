//! EE account update proof implementation wrapping `ee-acct-runtime` with zkaleido proof IO.

use std::sync::Arc;

use alpen_ee_da_runtime::verification::verify_da_witness;
use alpen_ee_da_types::ArchivedDaWitness;
use alpen_ee_params::{AlpenParams, AlpenSpecId};
use alpen_reth_evm::evm::AlpenEvmFactory;
use reth_chainspec::ChainSpec;
use rkyv::rancor::Error as RkyvError;
use strata_ee_acct_runtime::ArchivedEePrivateInput;
use strata_ee_acct_types::EeAccountState;
use strata_evm_ee::EvmExecutionEnvironment;
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::ArchivedPrivateInput as ArchivedUpdatePrivateInput;
use zkaleido::ZkVmEnvSerde;

mod program;

pub use program::{EeAcctProgram, EeAcctProofInput};

/// Guest entry point for EE account update proof generation.
///
/// Verifies the account update against `params`'s genesis and bridge params
/// using the EVM execution environment, reading three rkyv-serialized
/// private inputs (EE, update, and DA witness) from the zkVM, and commits
/// the pre-encoded `UpdateProofPubParams` SSZ bytes as public output.
///
/// `params` and `chunk_predicate_key` are trusted, out-of-band arguments,
/// not zkVM input: genesis and bridge params are consensus-critical, and a
/// host-supplied predicate key would let a malicious prover bypass chunk
/// proof verification, so both are bound into this guest's verifying key
/// rather than trusted as prover-supplied private input. See
/// `provers/sp1/guest-alpen-acct/src/main.rs` for the guest-side
/// construction path.
pub fn process_ee_acct_update(
    zkvm: &impl ZkVmEnvSerde,
    params: &AlpenParams,
    chunk_predicate_key: &PredicateKey,
) {
    // TODO(STR-4002): pin to v0 until per-block version resolution is
    // threaded through the proof guests.
    let chain_spec: Arc<ChainSpec> = params.evm_spec().chain_spec(AlpenSpecId::V0).clone();

    let ee_buf = zkvm.read_buf();
    let ee_input: &ArchivedEePrivateInput =
        rkyv::access::<ArchivedEePrivateInput, RkyvError>(&ee_buf)
            .expect("failed to access rkyv EE archive");

    let upd_buf = zkvm.read_buf();
    let upd_input: &ArchivedUpdatePrivateInput =
        rkyv::access::<ArchivedUpdatePrivateInput, RkyvError>(&upd_buf)
            .expect("failed to access rkyv update archive");

    let evm_factory = AlpenEvmFactory::from_bridge_params(params.bridge_params());

    let da_buf = zkvm.read_buf();
    let da_witness: &ArchivedDaWitness = rkyv::access::<ArchivedDaWitness, RkyvError>(&da_buf)
        .expect("failed to access rkyv DA witness archive");

    let ee = EvmExecutionEnvironment::new(chain_spec, evm_factory);

    strata_ee_acct_runtime::verify_and_process_update(
        &ee,
        chunk_predicate_key,
        ee_input,
        upd_input,
    )
    .expect("account update verification failed");

    let update_pub_params = upd_input
        .try_decode_update_pub_params()
        .expect("failed to decode update public params");
    let pre_account_state: EeAccountState = upd_input
        .try_decode_pre_state()
        .expect("failed to decode EE account pre-state");
    let expected_pre_state_root = pre_account_state.last_exec_state_root().0;
    verify_da_witness(
        ee_input,
        da_witness,
        &update_pub_params,
        expected_pre_state_root,
    )
    .expect("DA witness verification failed");

    // Pass through the pre-encoded SSZ bytes directly (zero-copy).
    zkvm.commit_buf(upd_input.update_pub_params_ssz());
}
