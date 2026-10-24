//! High-level entrypoint functions for EE account update processing.

use alpen_acct_types::EeAccountState;
use strata_predicate::PredicateKey;
use strata_snark_acct_runtime::ArchivedPrivateInput as ArchivedUpdatePrivateInput;
use strata_snark_acct_types::UpdateManifest;

use crate::{
    EeSnarkAccountProgram, EeVerificationInput, errors::AcctProgramResult,
    private_input::ArchivedPrivateInput as ArchivedEePrivateInput,
};

/// Verifies an update using the private inputs containing the account data
/// and the chunk proofs.
pub fn verify_and_process_update(
    chunk_predicate_key: &PredicateKey,
    ee_priv_input: &ArchivedEePrivateInput,
    upd_priv_input: &ArchivedUpdatePrivateInput,
) -> AcctProgramResult<()> {
    // 1. Construct verification input.
    let vinput = EeVerificationInput::new(chunk_predicate_key, ee_priv_input.chunks());

    // 2. Construct the program instance and call out to the general update
    // processing.
    let prog = EeSnarkAccountProgram::new();
    strata_snark_acct_runtime::verify_and_process_update(&prog, upd_priv_input, vinput)?;

    Ok(())
}

/// Processes changes to an account's inner state using an update manifest,
/// presumably fetched from RPCs or L1.
///
/// The post-state assertion runs iff the manifest carries an expected
/// `new_state_root` (see [`UpdateManifest`]).
pub fn process_update_unconditionally(
    state: &mut EeAccountState,
    update_manifest: &UpdateManifest,
) -> AcctProgramResult<()> {
    // 1. Construct the program instance and call out to the general update
    // processing.
    let prog = EeSnarkAccountProgram::new();
    strata_snark_acct_runtime::apply_update_unconditionally(&prog, state, update_manifest)?;

    Ok(())
}
