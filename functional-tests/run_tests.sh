#!/bin/bash
set -e

export RUST_BACKTRACE=1
export RUST_LOG="debug,sled=warn,hyper=warn,h2=warn,soketto=warn,jsonrpsee-server=warn,mio=warn"

# Which EE prover backend the built alpen-client is exercised against.
#   native (default): debug build, no real ZK proving. Fast iteration.
#   sp1:              release build, real compiled SP1 guest proving. Real
#                     SP1 proving is unusably slow in debug.
EE_PROVER_BACKEND="${EE_PROVER_BACKEND:-native}"
case "$EE_PROVER_BACKEND" in
  native | sp1) ;;
  *)
    echo "Unknown EE_PROVER_BACKEND: $EE_PROVER_BACKEND (expected: native|sp1)" >&2
    exit 1
    ;;
esac
export EE_PROVER_BACKEND

# Real proving takes minutes, not the near-instant native signing, so Reth's
# chatty engine/trie debug logs pile up enough to drown the service log.
if [ "$EE_PROVER_BACKEND" = sp1 ]; then
  RUST_LOG="$RUST_LOG,engine::tree=info,trie=info,payload_builder=info"
  export RUST_LOG
fi

# Sets up PATH for built binaries.
setup_path() {
    if [ "$CARGO_RELEASE" = 1 ] || [ "$EE_PROVER_BACKEND" = sp1 ]; then
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
    if [ "$EE_PROVER_BACKEND" = sp1 ]; then
      cargo build --release --bin alpen-client
    else
      cargo build --bin alpen-client
    fi
}

# Builds the strata binaries from the git rev pinned in the root Cargo.toml
# and puts them on PATH ahead of any stale workspace-built ones.
build_strata() {
    local strata_bin_dir
    if [ "$EE_PROVER_BACKEND" = sp1 ]; then
      strata_bin_dir="$(CARGO_RELEASE=1 ./build_strata_bins.sh)"
    else
      strata_bin_dir="$(./build_strata_bins.sh)"
    fi
    export PATH="$strata_bin_dir:$PATH"
}

# Builds the real SP1 guest pair EE_PROVER_BACKEND=sp1 proves against. No-op
# under native. common/prover_backend.py knows where the artifacts land.
build_sp1_guests() {
    [ "$EE_PROVER_BACKEND" = sp1 ] || return 0

    # gen_sp1_guest_params.py shells out to strata-datatool, so the venv (and
    # PATH from build_strata above) must already be ready.
    uv sync

    local alpen_params
    alpen_params="$(uv run python -m scripts.gen_sp1_guest_params)"
    SP1_ALPEN_PARAMS_PATH="$alpen_params" cargo build --release -p strata-sp1-guest-builder
}

# Runs tests.
run_tests() {
    uv sync
    uv run entry.py "$@"
}

setup_path
build
build_strata
build_sp1_guests
run_tests "$@"
