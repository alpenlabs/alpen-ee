//! Exposes the SP1 guest ELFs compiled by this crate's build script.
//!
//! The build script emits ELFs (and other generated artifacts) to `<crate>/generated/`.
//! Accessing these static items panics if no ELF is present there yet, e.g. on a machine
//! that built with `SP1_SKIP_PROGRAM_BUILD=true` or without `SP1_ALPEN_PARAMS_PATH` set
//! (the build script's new default: no ELF is built unless explicitly asked for one — see
//! `build.rs`). If an ELF from an earlier guest revision is already present, it is loaded
//! as-is — neither of those skip a rebuild verifying it still matches current source.

use std::{fs, sync::LazyLock};

/// Directory where the build script emits generated artifacts, including ELFs.
const GENERATED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/generated");

pub static GUEST_ALPEN_CHUNK_ELF: LazyLock<Vec<u8>> =
    LazyLock::new(|| read_elf("guest-alpen-chunk"));

pub static GUEST_ALPEN_ACCT_ELF: LazyLock<Vec<u8>> = LazyLock::new(|| read_elf("guest-alpen-acct"));

fn read_elf(program: &str) -> Vec<u8> {
    let path = format!("{GENERATED_DIR}/{program}.elf");
    fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read guest ELF {path}: {e}; rebuild with SP1_ALPEN_PARAMS_PATH set and \
             without SP1_SKIP_PROGRAM_BUILD"
        )
    })
}
