//! The official EIP-2537 test vectors, run through revm's precompile entry
//! points with [`Bls12381Crypto`](alpen_reth_bls_crypto::Bls12381Crypto)
//! installed.
//!
//! The files are copied from
//! <https://github.com/ethereum/EIPs/tree/faaa35fff856/assets/eip-2537>.
//! `msm_G1_bls.json` and `msm_G2_bls.json` are left out for their size
//! (10 MB together). The `mul_*` files are single-pair MSM inputs and stand
//! in for them.

use std::sync::Once;

use bls12_381 as _;
use evm2 as _;
use proptest as _;
use revm::precompile::{
    bls12_381::{g1_add, g1_msm, g2_add, g2_msm, map_fp2_to_g2, map_fp_to_g1, pairing},
    EthPrecompileResult,
};
use serde_json::Value;

type PrecompileFn = fn(&[u8], u64) -> EthPrecompileResult;

static INSTALL: Once = Once::new();

fn install() {
    INSTALL.call_once(|| {
        assert!(
            alpen_reth_bls_crypto::install(),
            "backend already installed"
        )
    });
}

fn field<'a>(case: &'a Value, key: &str) -> &'a str {
    case[key]
        .as_str()
        .unwrap_or_else(|| panic!("vector without {key}: {case}"))
}

/// Checks each success vector's output and gas.
fn check_success(file: &str, json: &str, precompile: PrecompileFn) {
    install();
    let cases: Vec<Value> = serde_json::from_str(json).unwrap();
    assert!(!cases.is_empty(), "{file} is empty");
    for case in &cases {
        let name = field(case, "Name");
        let input = hex::decode(field(case, "Input")).unwrap();
        let out = precompile(&input, u64::MAX)
            .unwrap_or_else(|halt| panic!("{file} {name}: halted with {halt:?}"));
        assert_eq!(
            hex::encode(&out.bytes),
            field(case, "Expected"),
            "{file} {name}"
        );
        assert_eq!(
            out.gas_used,
            case["Gas"].as_u64().unwrap(),
            "{file} {name}: gas"
        );
    }
}

/// Checks that each failure vector halts.
fn check_failure(file: &str, json: &str, precompile: PrecompileFn) {
    install();
    let cases: Vec<Value> = serde_json::from_str(json).unwrap();
    assert!(!cases.is_empty(), "{file} is empty");
    for case in &cases {
        let name = field(case, "Name");
        let input = hex::decode(field(case, "Input")).unwrap();
        assert!(
            precompile(&input, u64::MAX).is_err(),
            "{file} {name}: did not halt"
        );
    }
}

macro_rules! vectors {
    ($($test:ident: $check:ident($file:literal, $precompile:path);)*) => {$(
        #[test]
        fn $test() {
            $check(
                $file,
                include_str!(concat!("../test-vectors/eip-2537/", $file)),
                $precompile,
            );
        }
    )*};
}

vectors! {
    add_g1: check_success("add_G1_bls.json", g1_add::g1_add);
    add_g2: check_success("add_G2_bls.json", g2_add::g2_add);
    mul_g1: check_success("mul_G1_bls.json", g1_msm::g1_msm);
    mul_g2: check_success("mul_G2_bls.json", g2_msm::g2_msm);
    pairing_check: check_success("pairing_check_bls.json", pairing::pairing);
    map_fp_to_g1: check_success("map_fp_to_G1_bls.json", map_fp_to_g1::map_fp_to_g1);
    map_fp2_to_g2: check_success("map_fp2_to_G2_bls.json", map_fp2_to_g2::map_fp2_to_g2);

    fail_add_g1: check_failure("fail-add_G1_bls.json", g1_add::g1_add);
    fail_add_g2: check_failure("fail-add_G2_bls.json", g2_add::g2_add);
    fail_mul_g1: check_failure("fail-mul_G1_bls.json", g1_msm::g1_msm);
    fail_mul_g2: check_failure("fail-mul_G2_bls.json", g2_msm::g2_msm);
    fail_msm_g1: check_failure("fail-msm_G1_bls.json", g1_msm::g1_msm);
    fail_msm_g2: check_failure("fail-msm_G2_bls.json", g2_msm::g2_msm);
    fail_pairing_check: check_failure("fail-pairing_check_bls.json", pairing::pairing);
    fail_map_fp_to_g1: check_failure("fail-map_fp_to_G1_bls.json", map_fp_to_g1::map_fp_to_g1);
    fail_map_fp2_to_g2: check_failure("fail-map_fp2_to_G2_bls.json", map_fp2_to_g2::map_fp2_to_g2);
}
