# SP1 guest builder

Builds the SP1 guest programs (`guest-alpen-chunk`, `guest-alpen-acct`) and exposes
the compiled ELFs to host crates via `strata_sp1_guest_builder::GUEST_*_ELF`.

## Building

Building this crate compiles the guest programs, which requires the
[SP1 toolchain](https://docs.succinct.xyz/docs/sp1/getting-started/install):

```sh
cargo build --release -p strata-sp1-guest-builder
```

To build without the SP1 toolchain (e.g. when running workspace-wide tests or
docs), skip guest compilation with `SP1_SKIP_PROGRAM_BUILD=true`. Clippy skips
it automatically.

ELFs are written to `provers/sp1/elfs/` (gitignored), a stable location that
survives `cargo clean`.

## Guest dependencies

The account guest recursively verifies chunk proofs, so the guests are built
sequentially: the chunk guest is compiled first, its Groth16 verifying-key
condition bytes are code-generated into `guest-alpen-acct/src/vks.rs`
(gitignored), and then the account guest is compiled with that key embedded.

## Features and environment variables

- `docker-build` — compile the guests inside Docker for reproducible ELFs.
- `SP1_SKIP_PROGRAM_BUILD=true` — skip guest compilation.
