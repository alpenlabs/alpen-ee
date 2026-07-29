//! Exposes the SP1 guest ELFs compiled by this crate's build script.
//!
//! The build script emits ELFs to `<crate>/elfs/`. Accessing these static items panics
//! if no ELF is present there yet, e.g. on a machine that has only ever built with
//! `SP1_SKIP_PROGRAM_BUILD=true`. If an ELF from an earlier guest revision is already
//! present, it is loaded as-is — `SP1_SKIP_PROGRAM_BUILD=true` does not verify that a
//! previously built ELF still matches the current guest source.

use std::{fs, sync::LazyLock};

/// Directory where the build script emits compiled guest ELFs.
const ELFS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/elfs");

pub static GUEST_ALPEN_CHUNK_ELF: LazyLock<Vec<u8>> =
    LazyLock::new(|| read_elf("guest-alpen-chunk"));

pub static GUEST_ALPEN_ACCT_ELF: LazyLock<Vec<u8>> = LazyLock::new(|| read_elf("guest-alpen-acct"));

fn read_elf(program: &str) -> Vec<u8> {
    let path = format!("{ELFS_DIR}/{program}.elf");
    fs::read(&path).unwrap_or_else(|e| {
        panic!("cannot read guest ELF {path}: {e}; rebuild without SP1_SKIP_PROGRAM_BUILD")
    })
}
