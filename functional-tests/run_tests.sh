#!/bin/bash
set -e

export RUST_BACKTRACE=1
export RUST_LOG="debug,sled=warn,hyper=warn,h2=warn,soketto=warn,jsonrpsee-server=warn,mio=warn"

# EE chunk/acct prover backend for the alpen-client sequencer: `native`
# (default, zkaleido NativeHost, no real proofs) or `sp1` (real SP1 Groth16
# proving, requires the SP1 toolchain: https://docs.succinct.xyz/docs/sp1/getting-started/install).
# Read by factories/alpen_client.py; forwarded here only to decide the build.
export EE_PROVER_BACKEND="${EE_PROVER_BACKEND:-native}"

# Real SP1 proving is unusably slow in a debug build, so force release
# whenever the sp1 backend is requested.
use_release_build() {
    [ "$CARGO_RELEASE" = 1 ] || [ "$EE_PROVER_BACKEND" = "sp1" ]
}

# Sets up PATH for built binaries.
setup_path() {
    if use_release_build; then
      # shellcheck disable=2155
      export PATH=$(realpath ../target/release/):$PATH
    else
      # shellcheck disable=2155
      export PATH=$(realpath ../target/debug/):$PATH
    fi
}

# Builds the alpen binaries from this workspace.
build() {
    # TODO(STR-3692): different binaries for sequencer and full nodes
    if use_release_build; then
      cargo build --release --bin alpen-client
    else
      cargo build --bin alpen-client
    fi
}

# Builds the real SP1 guest ELFs (chunk + acct) when EE_PROVER_BACKEND=sp1,
# so factories/alpen_client.py finds them under provers/sp1/elfs/.
build_sp1_guests() {
    if [ "$EE_PROVER_BACKEND" = "sp1" ]; then
      cargo build --release -p strata-sp1-guest-builder
    fi
}

# Builds the strata binaries from the git rev pinned in the root Cargo.toml
# and puts them on PATH ahead of any stale workspace-built ones.
build_strata() {
    local strata_bin_dir
    strata_bin_dir="$(./build_strata_bins.sh)"
    export PATH="$strata_bin_dir:$PATH"
}

# Runs tests.
run_tests() {
    uv sync
    uv run entry.py "$@"
}

setup_path
build
build_sp1_guests
build_strata
run_tests "$@"
