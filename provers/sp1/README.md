# SP1 guest builder

Builds the SP1 guest programs (`guest-alpen-chunk`, `guest-alpen-acct`) and exposes
the compiled ELF paths to host crates via `strata_sp1_guest_builder::GUEST_*_ELF_PATH`.

## Building

Building this crate compiles the guest programs, which requires the
[SP1 toolchain](https://docs.succinct.xyz/docs/sp1/getting-started/install):

```sh
cargo build --release -p strata-sp1-guest-builder
```

To build without the SP1 toolchain (e.g. when running workspace-wide tests or
docs), skip guest compilation with `SP1_SKIP_PROGRAM_BUILD=true`. Clippy skips
it automatically.

ELFs are written to `provers/sp1/generated/` (gitignored), a stable location
that survives `cargo clean`.

## Guest dependencies

The account guest recursively verifies chunk proofs, so the guests are built
sequentially: the chunk guest is compiled first, its Groth16 predicate
condition bytes are code-generated into `guest-alpen-acct/src/predicates.rs`
(gitignored), and then the account guest is compiled with that predicate
embedded.

## Features and environment variables

- `docker-build` — compile the guests inside Docker for reproducible ELFs.
- `SP1_SKIP_PROGRAM_BUILD=true` — skip guest compilation.
