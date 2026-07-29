#![no_main]
zkaleido_sp1_guest_env::entrypoint!(main);

mod predicates;

use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_proofimpl_alpen_acct::process_ee_acct_update;
use zkaleido_sp1_guest_env::Sp1ZkVmEnv;

/// Constructs the chunk proof predicate key from the Groth16 predicate
/// condition bytes that `build.rs` embeds in `predicates.rs`.
fn chunk_predicate_key() -> PredicateKey {
    PredicateKey::new(
        PredicateTypeId::Sp1Groth16,
        predicates::ALPEN_CHUNK_PREDICATE_CONDITION_BYTES.to_vec(),
    )
}

fn main() {
    let key = chunk_predicate_key();
    process_ee_acct_update(&Sp1ZkVmEnv, &key)
}
