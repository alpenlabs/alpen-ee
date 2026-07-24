# SP1 guest builder

Builds the SP1 guest programs (`guest-alpen-chunk`, `guest-alpen-acct`) and exposes
the compiled ELFs to host crates via `strata_sp1_guest_builder::GUEST_*_ELF`.

## Building

Guest compilation only happens in release builds with the `sp1-dev` feature and
requires the [SP1 toolchain](https://docs.succinct.xyz/docs/sp1/getting-started/install):

```sh
cargo build --release -p strata-sp1-guest-builder --features sp1-dev
```

Plain debug builds (and clippy) skip guest compilation entirely, so everyday
development doesn't need the SP1 toolchain.

ELFs are written to `provers/sp1/elfs/` (gitignored), a stable location that
survives `cargo clean`.

## Guest dependencies

The account guest recursively verifies chunk proofs, so the guests are built
sequentially: the chunk guest is compiled first, its Groth16 verifying-key
condition bytes are code-generated into `guest-alpen-acct/src/vks.rs`
(gitignored), and then the account guest is compiled with that key embedded.

## Features and environment variables

- `sp1-dev` — actually build the guests (see above).
- `docker-build` — compile the guests inside Docker for reproducible ELFs.
- `ZKVM_MOCK=1` — build the guests with `mock-verify` so recursive proof
  verification is a no-op. Testing only; never use in production.
