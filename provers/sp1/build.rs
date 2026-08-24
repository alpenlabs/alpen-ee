//! Build script for the SP1 guest programs (`guest-alpen-chunk`, `guest-alpen-acct`).
//!
//! Guests are built sequentially in dependency order: the account guest verifies chunk
//! proofs, so the chunk guest's Groth16 predicate condition bytes are code-generated
//! between the two builds. Both guests also embed the `AlpenParams` (genesis,
//! bridge params) baked in from an external JSON file — see below. All generated
//! artifacts (ELFs, `predicates.rs`, `alpen_params.rs`) are emitted to `<crate>/generated/`.
//!
//! Building the guests requires the SP1 toolchain; set `SP1_SKIP_PROGRAM_BUILD=true` to
//! skip guest compilation entirely (clippy is skipped automatically).
//!
//! Even without that, the guest build only runs when `SP1_ALPEN_PARAMS_PATH` is set —
//! this build script runs on every `cargo build`/`check`/`test` that touches this crate,
//! and building a real, provable guest ELF now requires external params input. Rather
//! than fail (or silently bake in a default) whenever that input is missing, the build
//! opts out by default: no ELF is produced, and downstream code that needs one (`lib.rs`)
//! panics with a clear message pointing at this variable.
//!
//! # Features
//!
//! - **`docker-build`** — compile the guests inside Docker via `build_program_with_args` for
//!   reproducible ELFs. The output location is unchanged.
//!
//! # Environment variables
//!
//! - **`SP1_SKIP_PROGRAM_BUILD`** — when set to `true`, skip guest compilation entirely (the same
//!   variable sp1-build honors internally). Takes precedence over `SP1_ALPEN_PARAMS_PATH`.
//! - **`SP1_ALPEN_PARAMS_PATH`** — absolute path to an `AlpenParams` JSON artifact (the same format
//!   `alpen-client --alpen-params` loads) to bake into both guest ELFs. Required to actually build
//!   the guests; unset means "skip" — any ELF already in `generated/` from an earlier build is left
//!   in place, the same as `SP1_SKIP_PROGRAM_BUILD`. Must be absolute: build scripts run with CWD
//!   set to this crate's own directory, not the invocation directory, so a relative path would
//!   resolve against the wrong base.

use std::{env, fs, path::Path};

use sp1_build::{build_program_with_args, BuildArgs};
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    HashableKey, ProvingKey,
};
use sp1_verifier::{GROTH16_VK_BYTES, VK_ROOT_BYTES};
use zkaleido_sp1_groth16_verifier::SP1Groth16Verifier;

const GENERATED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/generated");

const ALPEN_CHUNK: &str = "guest-alpen-chunk";
const ALPEN_ACCT: &str = "guest-alpen-acct";

fn main() {
    println!("cargo:rerun-if-env-changed=SP1_SKIP_PROGRAM_BUILD");
    println!("cargo:rerun-if-env-changed=SP1_ALPEN_PARAMS_PATH");

    if skip_elf_build() {
        println!(
            "cargo:warning=SP1_SKIP_PROGRAM_BUILD set or clippy detected; skipping guest build"
        );
        return;
    }

    let Some(params_path) = env::var_os("SP1_ALPEN_PARAMS_PATH") else {
        println!(
            "cargo:warning=SP1_ALPEN_PARAMS_PATH not set; skipping SP1 guest build (point it at \
             an alpen-params.json to build real, provable guest ELFs)"
        );
        return;
    };
    let params_path = Path::new(&params_path);
    // Build scripts run with CWD set to the crate root (`provers/sp1/`), not
    // the directory `cargo build` was invoked from — a relative path here
    // would silently resolve against the wrong base, so require an absolute
    // one and fail loudly rather than guess.
    if !params_path.is_absolute() {
        panic!(
            "SP1_ALPEN_PARAMS_PATH must be an absolute path (build scripts run with CWD set to \
             the crate root, not the invocation directory) — got {}",
            params_path.display()
        );
    }
    println!("cargo:rerun-if-changed={}", params_path.display());

    fs::create_dir_all(GENERATED_DIR).unwrap_or_else(|e| panic!("create {GENERATED_DIR}: {e}"));

    // Written once, before either guest is built — both `guest-alpen-chunk`
    // and `guest-alpen-acct` need the identical `AlpenParams`, so generate it
    // in one shared location instead of duplicating the write per guest.
    write_alpen_params_const(params_path);

    // The account guest embeds the chunk guest's predicate condition, so the
    // chunk guest must be built (and its VK derived) first.
    build_guest(ALPEN_CHUNK);
    write_chunk_predicate_const(&sp1_predicate(ALPEN_CHUNK));
    build_guest(ALPEN_ACCT);

    // The acct guest's own predicate, unlike the chunk one above, is not
    // consumed by any guest. It is what the OL checks account proofs
    // against: the OL holds it as the account's `update_vk` and rejects any
    // update whose proof does not verify under it.
    write_acct_predicate_file(&sp1_predicate(ALPEN_ACCT));
}

fn build_guest(program: &str) {
    let build_args = BuildArgs {
        output_directory: Some(GENERATED_DIR.to_owned()),
        elf_name: Some(format!("{program}.elf")),
        #[cfg(feature = "docker-build")]
        docker: true,
        // Override the guest's workspace root with the repo root so Docker
        // mounts the entire workspace and the guest can import local crates.
        #[cfg(feature = "docker-build")]
        workspace_directory: Some("../../".to_owned()),
        ..Default::default()
    };
    build_program_with_args(program, build_args);
}

/// Derives the condition bytes of `program`'s `Sp1Groth16` predicate from its
/// freshly built ELF. These are the canonical uncompressed encoding of an
/// [`SP1Groth16Verifier`] (embedding the guest's verifying key) that the
/// runtime predicate verifier in `strata-predicate` decodes via
/// `SP1Groth16Verifier::parse`.
fn sp1_predicate(program: &str) -> Vec<u8> {
    let elf_path = Path::new(GENERATED_DIR).join(format!("{program}.elf"));
    let elf = fs::read(&elf_path)
        .unwrap_or_else(|e| panic!("read built ELF {}: {e}", elf_path.display()));

    let prover = ProverClient::builder().cpu().build();
    let pk = prover
        .setup(elf.into())
        .unwrap_or_else(|e| panic!("sp1 key setup for {program}: {e}"));
    let vkey_hash = pk.verifying_key().bytes32_raw();

    let verifier = SP1Groth16Verifier::load(&GROTH16_VK_BYTES, vkey_hash, *VK_ROOT_BYTES, true)
        .unwrap_or_else(|e| panic!("load SP1 Groth16 verifier: {e}"));
    verifier.to_uncompressed_bytes()
}

/// Writes the acct guest's own predicate to `<generated>/alpen-acct.predicate`
/// as a plain `Sp1Groth16:<hex>` string — the same `"{TypeName}:{hex}"`
/// format `strata-predicate`'s serde impl parses, so it drops straight into
/// an OL params document. A text file rather than a generated Rust constant
/// because its consumers are outside the Rust build: the functional-test
/// harness reads it to configure genesis, and it is the value an operator
/// needs when standing up a chain against these ELFs.
fn write_acct_predicate_file(condition: &[u8]) {
    let hex = condition
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let out_path = Path::new(GENERATED_DIR).join("alpen-acct.predicate");
    fs::write(&out_path, format!("Sp1Groth16:{hex}"))
        .unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
}

/// Writes the chunk guest's predicate condition into `generated/predicates.rs`
/// so the account guest can construct the chunk predicate key at compile time
/// without pulling in heavy SP1 SDK dependencies. The predicate type id stays
/// hard-coded in the guest source; only the build-derived condition bytes are
/// generated.
fn write_chunk_predicate_const(condition: &[u8]) {
    // Plain `//` comments, not `//!` inner doc comments: this file is spliced
    // in via the `include!` macro (not loaded as its own module file via
    // `mod predicates;`), and `//!` is only valid at the true start of a
    // file/module — `include!` doesn't give it that context.
    let content = format!(
        "// Generated by `build.rs` — do not edit.\n\
         pub const ALPEN_CHUNK_PREDICATE_CONDITION_BYTES: &[u8] = &{condition:?};\n"
    );
    let out_path = Path::new(GENERATED_DIR).join("predicates.rs");
    fs::write(&out_path, content).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
}

/// Reads the `AlpenParams` JSON at `path`, validates it parses, and writes
/// `generated/alpen_params.rs`: the re-serialized JSON embedded as a string
/// literal (decoded by each guest at startup via `serde_json`), which also
/// doubles as a compact one-line doc comment for a quick, at-a-glance record
/// of what got baked in.
///
/// `AlpenParams` can't be `bincode`-encoded here to avoid that guest-side
/// JSON parse: `alloy_genesis::ChainConfig` uses `#[serde(flatten, default)]`
/// for forward-compatible extra fields, which requires a self-describing
/// format — bincode's non-self-describing binary encoding rejects it
/// (`SequenceMustHaveLength`). JSON stays the wire format end to end.
fn write_alpen_params_const(path: &Path) {
    let json = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let params: alpen_ee_params::AlpenParams = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("parse alpen params at {}: {e}", path.display()));
    let json_oneline = serde_json::to_string(&params).expect("re-serialize alpen params");

    println!(
        "cargo:warning=SP1 guest ELFs baking in alpen params from {}",
        path.display()
    );

    // Embed the JSON content itself as a string literal rather than
    // `include_str!`-ing the host path: under the `docker-build` feature the
    // guest compiles inside a container where the host's absolute path
    // doesn't exist, so `include_str!` would fail to resolve it.
    //
    // Plain `//` comments, not `//!` inner doc comments — see the comment in
    // `write_chunk_predicate_const` for why: this file is spliced in via
    // `include!`, which doesn't give `//!` the file/module context it requires.
    let content = format!(
        "// Generated by `build.rs` — do not edit.\n\
         pub const ALPEN_PARAMS_JSON: &str = {json_oneline:?};\n"
    );
    let out_path = Path::new(GENERATED_DIR).join("alpen_params.rs");
    fs::write(&out_path, content).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
}

/// Returns `true` when sp1-build itself would skip the build — under
/// `SP1_SKIP_PROGRAM_BUILD=true` or `cargo clippy`.
fn skip_elf_build() -> bool {
    let skip_env = env::var("SP1_SKIP_PROGRAM_BUILD")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let is_clippy = env::var("RUSTC_WORKSPACE_WRAPPER")
        .map(|v| v.contains("clippy-driver"))
        .unwrap_or(false);
    skip_env || is_clippy
}
