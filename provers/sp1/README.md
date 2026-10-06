# SP1 guest builder

Builds the SP1 guest programs (`guest-alpen-chunk`, `guest-alpen-acct`) and exposes
the compiled ELF paths to host crates via `alpen_sp1_guest_builder::GUEST_*_ELF_PATH`.

## Building

Building this crate compiles the guest programs, which requires the
[SP1 toolchain](https://docs.succinct.xyz/docs/sp1/getting-started/install):

```sh
cargo build --release -p alpen-sp1-guest-builder
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
- `SP1_ALPEN_PARAMS_PATH` — absolute path to the params JSON baked into both
  guests, for example `$PWD/params/dev.json`. The guests are only built when
  this is set.

## Publishing

The params are baked into the guests, so each network has its own ELFs and
account predicate. Each network's params live in `params/<network>.json`.

`.github/workflows/publish-guests.yml` builds the guests for one network with
`docker-build` and publishes these files:

- `<network>-guest-alpen-chunk.elf` and `<network>-guest-alpen-acct.elf`
- `<network>-alpen-acct.predicate`, the account guest's `Sp1Groth16` predicate
  that the OL uses to check account proofs
- `<network>-alpen-params.json`, the params baked into both guests

The files always go to a workflow artifact. Pushing a `v*` tag runs
`.github/workflows/release.yml`. It creates a draft GitHub Release, runs
`publish-guests.yml` for every network in `params/` and attaches the files. It
then puts each network's account predicate in the release notes and publishes
the release.

After the release is published, `.github/workflows/publish-guests-s3.yml`
checks each file against the release attestation and copies it to
`s3://$ALPEN_GUESTS_S3_BUCKET/elfs/alpen-ee/<version>/`, with a `.sha256` file
next to each. `<version>` is `<commit sha8>-<params sha8>`, so networks with
the same params share a folder. The files there drop their network prefix, and
`manifest.json` lists the networks.

The copy waits for a reviewer to approve it in the `sp1-artifacts`
environment. It is skipped until the `ALPEN_GUESTS_S3_BUCKET` and
`ALPEN_GUESTS_S3_ROLE_ARN` repo variables are set.

To build with params that are not in the repo yet, run `publish-guests.yml` by
hand with `params_url`. Releases are immutable once published, so `release_tag`
only works while that release is still a draft.

To check published files, rebuild at the same commit with
`--features docker-build` and `SP1_ALPEN_PARAMS_PATH` pointing at the published
params, then compare digests.
