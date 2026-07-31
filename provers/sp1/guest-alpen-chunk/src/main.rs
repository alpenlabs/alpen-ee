#![no_main]
zkaleido_sp1_guest_env::entrypoint!(main);

use alpen_ee_params::AlpenParams;
use strata_proofimpl_alpen_chunk::process_ee_chunk;
use zkaleido_sp1_guest_env::Sp1ZkVmEnv;

/// Compile-time-baked build artifact: the canonical `AlpenParams` JSON,
/// embedded by `build.rs` from the file at `SP1_ALPEN_PARAMS_PATH`. Not
/// zkVM input — see `strata_proofimpl_alpen_chunk::process_ee_chunk` for why.
mod alpen_params {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../generated/alpen_params.rs"));
}

fn canonical_alpen_params() -> AlpenParams {
    serde_json::from_str(alpen_params::ALPEN_PARAMS_JSON)
        .expect("embedded canonical alpen params must parse")
}

fn main() {
    let params = canonical_alpen_params();
    process_ee_chunk(&Sp1ZkVmEnv, &params)
}
