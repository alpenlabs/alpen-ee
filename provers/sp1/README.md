# SP1 guest builder

This crate builds the two SP1 guest programs, `guest-alpen-chunk` and
`guest-alpen-acct`. Its build script writes their ELFs, and the account
guest's predicate and program ID, to `generated/`. Host crates find them
through the paths in `src/lib.rs`.

Each guest is its own Cargo workspace with its own `Cargo.lock`. Neither is a
member of the root workspace.

## Prerequisites

- **Rust.** The toolchain pinned in the root `rust-toolchain.toml`.
- **protoc.** The host SP1 crates compile protobuf files.
- **SP1 toolchain**, for a local build. Install it with `sp1up` and pin it to
  the version of the SP1 crates in the root `Cargo.lock` (currently `v6.8.1`).
  `-c` also installs the RISC-V C toolchain, which crates such as
  `secp256k1-sys` need to build for the guest:

  ```sh
  curl -fsSL https://sp1.succinct.xyz | bash
  sp1up -v v6.8.1 -c
  cargo prove --version
  ```

- **Docker**, for a [reproducible build](#reproducible-builds). Releases build
  this way, so you need Docker to reproduce a released ELF. The Docker daemon
  must be running.

## Outputs

All files land in `generated/`. It is gitignored.

| File                    | Contents                                                     |
|-------------------------|--------------------------------------------------------------|
| `guest-alpen-chunk.elf` | The chunk guest.                                             |
| `guest-alpen-acct.elf`  | The account guest.                                           |
| `alpen-acct.predicate`  | The account guest's predicate, as `Sp1Groth16:<hex>`.        |
| `alpen-acct.program-id` | The account guest's program ID, as `0x<hex>`.                |
| `alpen_params.rs`       | The params, embedded in the chunk guest.                     |
| `predicates.rs`         | The chunk guest's predicate, embedded in the account guest.  |

The OL holds the account predicate as the account's `update_vk` and checks
account proofs against it. The program ID is SP1's program vkey hash of the
account ELF, the value `cargo prove vkey --elf <file>` prints. The predicate
holds the program ID folded into one curve point, and its other bytes are the
same for every build. So compare program IDs to tell two builds apart.

`src/lib.rs` exposes `GUEST_ALPEN_CHUNK_ELF_PATH`, `GUEST_ALPEN_ACCT_ELF_PATH`
and `GUEST_ALPEN_ACCT_PREDICATE_PATH`.

## Building

The guests build only when `SP1_ALPEN_PARAMS_PATH` is set to the absolute
path of a params JSON. Without it, `cargo build` compiles the crate and skips
the guests, so workspace builds and tests don't need the SP1 toolchain. Build
them from the repo root:

```sh
SP1_ALPEN_PARAMS_PATH=$PWD/params/dev.json \
  cargo build --release --locked -p alpen-sp1-guest-builder
```

The path must be absolute, because build scripts run in the crate's own
directory and not the one you ran `cargo` from.

### Build order

The account guest checks chunk proofs, so the build script runs these steps
in order:

1. Write the params to `alpen_params.rs`.
2. Build the chunk guest and derive its verifying key.
3. Write the chunk guest's predicate to `predicates.rs`.
4. Build the account guest.
5. Derive the account guest's verifying key, then write its predicate and
   program ID.

So the account program ID depends on the chunk guest. A change to the chunk
guest alone still changes both guests' keys. Only the chunk guest reads the
params, but for the same reason they change both keys too.

### Guest lockfiles

Each guest builds with `--locked`. Before it compiles a guest, the build script
checks that the guest's `Cargo.lock` is up to date. If a change to a shared
crate needs a new lockfile entry, the build fails until you sync the lockfile
and commit it:

```sh
cargo update --workspace --manifest-path provers/sp1/guest-alpen-chunk/Cargo.toml
```

### Features and environment variables

- `docker-build` compiles the guests in Docker. See
  [Reproducible builds](#reproducible-builds).
- `SP1_ALPEN_PARAMS_PATH` is the absolute path of the params JSON. The guests
  build only when it is set.
- `SP1_SKIP_PROGRAM_BUILD=true` skips the guests, even when
  `SP1_ALPEN_PARAMS_PATH` is set. `cargo clippy` always skips them.
- `SP1_ALPEN_CHUNK_PROOF_PATH` makes the account guest accept proofs from the
  chunk program that made a saved proof, instead of the chunk guest built with
  it. It is for `prover-perf` only, see
  [`bin/prover-perf/README.md`](../../bin/prover-perf/README.md). A
  `docker-build` build refuses it.

## Reproducible builds

A plain build compiles the guests with your local SP1 toolchain. The result
can differ from machine to machine. To get the same ELFs as a release, add the
`docker-build` feature:

```sh
SP1_ALPEN_PARAMS_PATH=$PWD/params/dev.json \
  cargo build --release --locked -p alpen-sp1-guest-builder --features docker-build
```

This compiles each guest inside SP1's `ghcr.io/succinctlabs/sp1` image, with
the repo mounted at the same path every time. Only the compile runs in Docker.
The keys, the predicate and the program ID are still derived on the host, from
the ELFs.

Two Docker builds give the same files when these inputs match:

- The source at the commit, including every local crate the guests use.
- Each guest's `Cargo.lock`.
- The params file.
- The SP1 version. The image tag is `v` plus the `sp1-build` version in the
  root `Cargo.lock`, and the host derives the keys with `sp1-sdk` from the same
  lockfile.

The image tag follows `sp1-build`, so you don't pin it by hand. An SP1 bump
changes the image, and with it the ELFs and the keys. When you bump the SP1
crates, also bump `SP1_VERSION` in `.github/workflows/prover.yml`,
`.github/workflows/main-eest.yml` and `.github/workflows/publish-guests.yml`,
which install the SP1 toolchain in CI.

The image is `linux/amd64` only. On an Apple Silicon Mac, Docker runs it under
emulation, so the build is slower there.

### Things to watch out for

- `generated/` survives `cargo clean`. A build without `SP1_ALPEN_PARAMS_PATH`
  leaves old files in place, and they may not match the current source or
  params. Rebuild before you use them.
- A Docker build fails if `CARGO_TARGET_DIR` points outside the repo, because
  the container only sees the repo.

## Publishing

The params are baked into the guests, so each network has its own ELFs,
account program ID and predicate. Each network's params live in
`params/<network>.json`.

`.github/workflows/publish-guests.yml` builds the guests for one network with
`docker-build` and publishes these files:

- `<network>-guest-alpen-chunk.elf` and `<network>-guest-alpen-acct.elf`
- `<network>-alpen-acct.predicate`, the account guest's `Sp1Groth16` predicate
  that the OL uses to check account proofs
- `<network>-alpen-params.json`, the params baked into both guests

It also writes the account guest's program ID: SP1's program vkey hash of the
account ELF as `0x<hex>`, the value `cargo prove vkey --elf <file>` prints. The
predicate holds this program ID folded into one curve point, and its other
bytes are the same for every build, so compare program IDs to tell builds
apart. The program ID is not a release file. It goes in the run summary, the
`alpen-acct-program-id-<network>` workflow artifact and the release notes.

The files always go to a workflow artifact. Pushing a `v*` tag runs
`.github/workflows/release.yml`. It creates a draft GitHub Release, runs
`publish-guests.yml` for every network in `params/` except `dev` and attaches
the files. It then puts each network's account program ID and predicate in
the release notes. Once the draft holds exactly the files that were built, and
the tag still points at the commit they were built from, it publishes the
release.

After the release is published, `.github/workflows/publish-guests-s3.yml`
checks each file against the release attestation and copies it to
`s3://$ALPEN_GUESTS_S3_BUCKET/elfs/alpen-ee/<version>/`, with a `.sha256` file
next to each. `<version>` is `<commit sha8>-<params sha8>`, so networks with
the same params share a folder. The files there drop their network prefix, and
`manifest.json` lists the networks.

The copy waits for a reviewer to approve it in the `sp1-artifacts`
environment. It fails if the `ALPEN_GUESTS_S3_BUCKET` or
`ALPEN_GUESTS_S3_ROLE_ARN` repo variable is not set.

To build `dev`, run `publish-guests.yml` by hand. To build with params that are
not in the repo yet, run it with `params_url`. Releases are immutable once
published, so `release_tag` only works while that release is still a draft.

`.github/workflows/prover.yml` builds both guests with the `dev` params on
every PR, so a change that breaks the guest build fails there and not at
release time.

## Checking a release

A release holds each network's files under a `<network>-` prefix. Download one
network's files and check each against the release attestation:

```sh
gh release download <tag> --repo alpenlabs/alpen-ee --pattern 'testnet-*' --dir release
for f in release/*; do gh release verify-asset <tag> "$f" --repo alpenlabs/alpen-ee; done
```

Check that the account ELF matches the program ID in the release notes:

```sh
cargo prove vkey --elf release/testnet-guest-alpen-acct.elf
```

To check that the files match the source, check out the tag and do a
[reproducible build](#reproducible-builds) with the released params. Then
compare the outputs with the released files:

```sh
git checkout <tag>
SP1_ALPEN_PARAMS_PATH=$PWD/release/testnet-alpen-params.json \
  cargo build --release --locked -p alpen-sp1-guest-builder --features docker-build
for f in guest-alpen-chunk.elf guest-alpen-acct.elf alpen-acct.predicate; do
  cmp "provers/sp1/generated/$f" "release/testnet-$f" && echo "$f matches"
done
```
