//! Public artifact paths produced by this crate's build script.
//!
//! This crate exists solely to build two *test-only* real SP1 guest
//! candidates (`v0`, `v1`) for `test_ee_predicate_transition.py`'s
//! `--prover-backend sp1` variant — it deliberately does not touch the
//! production `strata-sp1-guest-builder` crate (`provers/sp1`), which CI and
//! docker depend on as-is.
//!
//! Artifacts are emitted into `<crate>/generated/{v0,v1}/` (see `build.rs`);
//! the constants below point at those stable paths. Reading a file at a path
//! requires it to already be present there, e.g. built with
//! `SP1_ALPEN_PARAMS_PATH_V0`/`SP1_ALPEN_PARAMS_PATH_V1` set (the build
//! script's default is to skip guest compilation entirely — see `build.rs`).

pub const GUEST_ALPEN_CHUNK_V0_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v0/guest-alpen-chunk-v0.elf"
);
pub const GUEST_ALPEN_ACCT_V0_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v0/guest-alpen-acct-v0.elf"
);
/// Plain-text `Sp1Groth16:<hex>` predicate string for the `v0` acct guest —
/// the same format `strata-predicate`'s serde impl parses, and the same
/// literal `--predicate`/genesis value the functional-test Python harness
/// consumes directly.
pub const GUEST_ALPEN_ACCT_V0_PREDICATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v0/acct-predicate.txt"
);

pub const GUEST_ALPEN_CHUNK_V1_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v1/guest-alpen-chunk-v1.elf"
);
pub const GUEST_ALPEN_ACCT_V1_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v1/guest-alpen-acct-v1.elf"
);
/// Same as [`GUEST_ALPEN_ACCT_V0_PREDICATE_PATH`], for the `v1` acct guest.
pub const GUEST_ALPEN_ACCT_V1_PREDICATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/v1/acct-predicate.txt"
);
