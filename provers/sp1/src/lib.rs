//! Public ELF path exports produced by this crate's build script.
//!
//! ELFs are emitted into `<crate>/generated/` (see `build.rs`); the constants
//! below point at those stable paths. Reading the file at a path requires an
//! ELF to already be present there, e.g. built with `SP1_ALPEN_PARAMS_PATH`
//! set (the build script's default is to skip guest compilation entirely —
//! see `build.rs`). If an ELF from an earlier guest revision is already
//! present, it is loaded as-is; neither case rebuilds to verify it still
//! matches the current source.

pub const GUEST_ALPEN_CHUNK_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/guest-alpen-chunk.elf"
);
pub const GUEST_ALPEN_ACCT_ELF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/generated/guest-alpen-acct.elf"
);
/// Plain-text `Sp1Groth16:<hex>` predicate for the account guest — the value
/// that verifies this build's account proofs, and so the genesis `update_vk`
/// a chain running these ELFs must be configured with. Written alongside the
/// ELFs by `build.rs`; same presence caveat as the paths above.
pub const GUEST_ALPEN_ACCT_PREDICATE_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/generated/alpen-acct.predicate");
