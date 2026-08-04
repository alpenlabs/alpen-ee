//! Build script for the test-only SP1 guest programs
//! (`guest-alpen-chunk-{v0,v1}`, `guest-alpen-acct-{v0,v1}`).
//!
//! This crate exists purely to give `test_ee_predicate_transition.py`'s
//! `--prover-backend sp1` variant two *genuinely distinct, real* compiled
//! guest programs to rotate `update_vk` between. A real SP1 predicate is
//! intrinsically tied to its compiled ELF — there is no signing-key knob like
//! the native backend has — so getting two distinct real predicates requires
//! two distinct compiled binaries. It deliberately does not touch the
//! production `strata-sp1-guest-builder` crate (`provers/sp1`), which CI and
//! docker depend on as-is.
//!
//! Mirrors `provers/sp1/build.rs`'s two-stage build (chunk first, its Groth16
//! predicate condition is code-generated for the acct guest to embed, then
//! acct) once per version, each version reading its own `AlpenParams` JSON so
//! the two acct guests compile to different verifying keys. Unlike
//! `provers/sp1/build.rs`, each acct guest's *own* Groth16 predicate is also
//! derived and written out as a plain `Sp1Groth16:<hex>` text file — this is
//! the literal genesis/rotation predicate value the functional-test Python
//! harness reads directly, with no CLI/cross-repo plumbing needed.
//!
//! Building the guests requires the SP1 toolchain; set `SP1_SKIP_PROGRAM_BUILD=true` to
//! skip guest compilation entirely (clippy is skipped automatically).
//!
//! Even without that, the guest build only runs when both
//! `SP1_ALPEN_PARAMS_PATH_V0` and `SP1_ALPEN_PARAMS_PATH_V1` are set — this
//! build script runs on every `cargo build`/`check`/`test` that touches this
//! crate, and building real, provable guest ELFs now requires external params
//! input. Rather than fail (or silently bake in a default) whenever that
//! input is missing, the build opts out by default: no ELF is produced.
//!
//! # Features
//!
//! - **`docker-build`** — compile the guests inside Docker via `build_program_with_args` for
//!   reproducible ELFs. The output location is unchanged.
//!
//! # Environment variables
//!
//! - **`SP1_SKIP_PROGRAM_BUILD`** — when set to `true`, skip guest compilation entirely (the same
//!   variable sp1-build honors internally). Takes precedence over the params-path variables below.
//! - **`SP1_ALPEN_PARAMS_PATH_V0`** / **`SP1_ALPEN_PARAMS_PATH_V1`** — absolute paths to two
//!   `AlpenParams` JSON artifacts (the same format `alpen-client --alpen-params` loads), baked
//!   into the `v0` and `v1` guest pairs respectively. Both are required to build any guests;
//!   either missing means "skip". Must be absolute: build scripts run with CWD set to this
//!   crate's own directory, not the invocation directory, so a relative path would resolve
//!   against the wrong base.

use std::{env, fs, path::Path};

use sp1_build::{build_program_with_args, BuildArgs};
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    HashableKey, ProvingKey,
};
use sp1_verifier::{GROTH16_VK_BYTES, VK_ROOT_BYTES};
use zkaleido_sp1_groth16_verifier::SP1Groth16Verifier;

const CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// One `{v0, v1}` guest pair's build inputs/outputs.
struct GuestVersion {
    /// `v0` or `v1` — also the `generated/{version}` subdirectory name.
    version: &'static str,
    /// Env var naming the `AlpenParams` JSON baked into this version's pair.
    params_env_var: &'static str,
    chunk_crate: &'static str,
    acct_crate: &'static str,
}

const VERSIONS: &[GuestVersion] = &[
    GuestVersion {
        version: "v0",
        params_env_var: "SP1_ALPEN_PARAMS_PATH_V0",
        chunk_crate: "guest-alpen-chunk-v0",
        acct_crate: "guest-alpen-acct-v0",
    },
    GuestVersion {
        version: "v1",
        params_env_var: "SP1_ALPEN_PARAMS_PATH_V1",
        chunk_crate: "guest-alpen-chunk-v1",
        acct_crate: "guest-alpen-acct-v1",
    },
];

fn main() {
    println!("cargo:rerun-if-env-changed=SP1_SKIP_PROGRAM_BUILD");
    for v in VERSIONS {
        println!("cargo:rerun-if-env-changed={}", v.params_env_var);
    }

    if skip_elf_build() {
        println!(
            "cargo:warning=SP1_SKIP_PROGRAM_BUILD set or clippy detected; skipping guest build"
        );
        return;
    }

    for v in VERSIONS {
        build_version(v);
    }
}

fn build_version(v: &GuestVersion) {
    let Some(params_path) = env::var_os(v.params_env_var) else {
        println!(
            "cargo:warning={} not set; skipping the {} SP1 guest pair (point it at an \
             alpen-params.json to build a real, provable guest ELF pair)",
            v.params_env_var, v.version
        );
        return;
    };
    let params_path = Path::new(&params_path);
    // Build scripts run with CWD set to the crate root, not the directory
    // `cargo build` was invoked from — a relative path here would silently
    // resolve against the wrong base, so require an absolute one and fail
    // loudly rather than guess.
    if !params_path.is_absolute() {
        panic!(
            "{} must be an absolute path (build scripts run with CWD set to the crate root, not \
             the invocation directory) — got {}",
            v.params_env_var,
            params_path.display()
        );
    }
    println!("cargo:rerun-if-changed={}", params_path.display());

    let generated_dir = format!("{CRATE_DIR}/generated/{}", v.version);
    fs::create_dir_all(&generated_dir).unwrap_or_else(|e| panic!("create {generated_dir}: {e}"));

    // Written once, before either guest in this version's pair is built —
    // both guests need the identical `AlpenParams`, so generate it in one
    // shared location instead of duplicating the write per guest.
    write_alpen_params_const(&generated_dir, params_path);

    // The account guest embeds the chunk guest's predicate condition, so the
    // chunk guest must be built (and its VK derived) first.
    build_guest(v.chunk_crate, &generated_dir);
    let chunk_condition = predicate_condition(&generated_dir, v.chunk_crate);
    write_acct_predicates(&generated_dir, &chunk_condition);
    build_guest(v.acct_crate, &generated_dir);

    // Unlike the production `provers/sp1/build.rs`, also derive and expose
    // the acct guest's *own* predicate: this is the literal genesis/rotation
    // value the functional-test Python harness needs, and there is no other
    // in-repo way to get it (datatool's `--alpen-predicate sp1-groth16` reads
    // a different, out-of-sync guest build entirely).
    let acct_condition = predicate_condition(&generated_dir, v.acct_crate);
    write_acct_predicate_hex(&generated_dir, &acct_condition);
}

fn build_guest(program: &str, generated_dir: &str) {
    let build_args = BuildArgs {
        output_directory: Some(generated_dir.to_owned()),
        elf_name: Some(format!("{program}.elf")),
        #[cfg(feature = "docker-build")]
        docker: true,
        // Override the guest's workspace root with the repo root so Docker
        // mounts the entire workspace and the guest can import local crates.
        #[cfg(feature = "docker-build")]
        workspace_directory: Some("../../../".to_owned()),
        ..Default::default()
    };
    build_program_with_args(program, build_args);
}

/// Derives the condition bytes of `program`'s `Sp1Groth16` predicate from its
/// freshly built ELF at `<generated_dir>/<program>.elf`. These are the
/// canonical uncompressed encoding of an [`SP1Groth16Verifier`] (embedding the
/// guest's verifying key) that the runtime predicate verifier in
/// `strata-predicate` decodes via `SP1Groth16Verifier::parse`.
fn predicate_condition(generated_dir: &str, program: &str) -> Vec<u8> {
    let elf_path = Path::new(generated_dir).join(format!("{program}.elf"));
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

/// Writes the chunk guest's predicate condition into `<generated_dir>/predicates.rs`
/// so the account guest can construct the chunk predicate key at compile time
/// without pulling in heavy SP1 SDK dependencies. The predicate type id stays
/// hard-coded in the guest source; only the build-derived condition bytes are
/// generated.
fn write_acct_predicates(generated_dir: &str, condition: &[u8]) {
    // Plain `//` comments, not `//!` inner doc comments: this file is spliced
    // in via the `include!` macro (not loaded as its own module file via
    // `mod predicates;`), and `//!` is only valid at the true start of a
    // file/module — `include!` doesn't give it that context.
    let content = format!(
        "// Generated by `build.rs` — do not edit.\n\
         pub const ALPEN_CHUNK_PREDICATE_CONDITION_BYTES: &[u8] = &{condition:?};\n"
    );
    let out_path = Path::new(generated_dir).join("predicates.rs");
    fs::write(&out_path, content).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
}

/// Writes the acct guest's own predicate as a plain `Sp1Groth16:<hex>` text
/// file — the same `"{TypeName}:{hex}"` format `strata-predicate`'s serde
/// impl parses. Read directly by the functional-test Python harness (as the
/// literal genesis/rotation predicate value), not by any Rust code.
fn write_acct_predicate_hex(generated_dir: &str, condition: &[u8]) {
    let hex = condition.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let out_path = Path::new(generated_dir).join("acct-predicate.txt");
    fs::write(&out_path, format!("Sp1Groth16:{hex}"))
        .unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
}

/// Reads the `AlpenParams` JSON at `path`, validates it parses, and writes
/// `<generated_dir>/alpen_params.rs`: the re-serialized JSON embedded as a
/// string literal (decoded by each guest at startup via `serde_json`), which
/// also doubles as a compact one-line doc comment for a quick, at-a-glance
/// record of what got baked in.
///
/// `AlpenParams` can't be `bincode`-encoded here to avoid that guest-side
/// JSON parse: `alloy_genesis::ChainConfig` uses `#[serde(flatten, default)]`
/// for forward-compatible extra fields, which requires a self-describing
/// format — bincode's non-self-describing binary encoding rejects it
/// (`SequenceMustHaveLength`). JSON stays the wire format end to end.
fn write_alpen_params_const(generated_dir: &str, path: &Path) {
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
    // `write_acct_predicates` for why: this file is spliced in via `include!`,
    // which doesn't give `//!` the file/module context it requires.
    let content = format!(
        "// Generated by `build.rs` — do not edit.\n\
         pub const ALPEN_PARAMS_JSON: &str = {json_oneline:?};\n"
    );
    let out_path = Path::new(generated_dir).join("alpen_params.rs");
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
