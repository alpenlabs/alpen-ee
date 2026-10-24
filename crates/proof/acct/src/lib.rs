//! EE account update proof implementation wrapping `acct-runtime` with zkaleido proof IO.

use alpen_acct_runtime::ArchivedEePrivateInput;
use alpen_acct_types::EeAccountState;
use alpen_da_runtime::verification::verify_da_witness;
use alpen_da_types::ArchivedDaWitness;
use alpen_params::AlpenSpecId;
use rkyv::rancor::Error as RkyvError;
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::ArchivedPrivateInput as ArchivedUpdatePrivateInput;
use zkaleido::ZkVmEnvSerde;

#[cfg(not(target_os = "zkvm"))]
mod program;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(not(target_os = "zkvm"))]
pub use program::{EeAcctProgram, EeAcctProofInput};

/// Guest entry point for EE account update proof generation.
///
/// Verifies the account update against the chunk proofs and the batch's DA,
/// reading three rkyv-serialized private inputs (EE, update, and DA witness)
/// from the zkVM, and commits the pre-encoded `UpdateProofPubParams` SSZ bytes
/// as public output.
///
/// The guest executes no blocks, so it takes no chain params. Each chunk proof
/// attests to execution under the params baked into the chunk guest, and
/// `chunk_predicate_key` pins that guest.
///
/// `spec_version` and `chunk_predicate_key` are trusted, out-of-band
/// arguments, not zkVM input: a host-supplied predicate key would let a
/// malicious prover bypass chunk proof verification, and a host-supplied spec
/// version would let one pick which rules its update is checked under --
/// claiming an older version for a newer batch to dodge a hardfork's rules.
/// Both are bound into this guest's verifying key instead, so the predicate
/// rotation that activates a version is also what authorizes proving under
/// it. See `provers/sp1/guest-alpen-acct/src/main.rs` for the guest-side
/// construction path.
///
/// One version per batch is well-defined because a rotation-consuming block
/// always ends its group -- see `sealing_policy::rotation_policy` -- so no
/// batch straddles an activation boundary.
pub fn process_ee_acct_update(
    zkvm: &impl ZkVmEnvSerde,
    spec_version: AlpenSpecId,
    chunk_predicate_key: &PredicateKey,
) {
    let ee_buf = zkvm.read_buf();
    let ee_input: &ArchivedEePrivateInput =
        rkyv::access::<ArchivedEePrivateInput, RkyvError>(&ee_buf)
            .expect("failed to access rkyv EE archive");

    let upd_buf = zkvm.read_buf();
    let upd_input: &ArchivedUpdatePrivateInput =
        rkyv::access::<ArchivedUpdatePrivateInput, RkyvError>(&upd_buf)
            .expect("failed to access rkyv update archive");

    let da_buf = zkvm.read_buf();
    let da_witness: &ArchivedDaWitness = rkyv::access::<ArchivedDaWitness, RkyvError>(&da_buf)
        .expect("failed to access rkyv DA witness archive");

    alpen_acct_runtime::verify_and_process_update(chunk_predicate_key, ee_input, upd_input)
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
        spec_version,
    )
    .expect("DA witness verification failed");

    // Pass through the pre-encoded SSZ bytes directly (zero-copy).
    zkvm.commit_buf(upd_input.update_pub_params_ssz());
}
