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

# Sets up PATH for built binaries.
setup_path() {
    if [ "$EE_PROVER_BACKEND" = sp1 ]; then
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

# Builds the two real SP1 guest candidates (v0, v1) test_ee_predicate_transition.py's
# sp1-backend variant rotates `update_vk` between, and exports their artifact
# paths for common/prover_backend.py to pick up. No-op under native.
build_sp1_guests() {
    [ "$EE_PROVER_BACKEND" = sp1 ] || return 0

    # gen_sp1_guest_params.py shells out to strata-datatool, so the venv (and
    # PATH from build_strata above) must already be ready.
    uv sync

    local fixtures_dir guests_dir
    fixtures_dir="$(realpath ../target)/sp1-guest-params"
    mkdir -p "$fixtures_dir"
    uv run python -m scripts.gen_sp1_guest_params --out-dir "$fixtures_dir"

    guests_dir="$(realpath ../provers/sp1-func-test-guests)"
    SP1_ALPEN_PARAMS_PATH_V0="$fixtures_dir/v0/alpen-params.json" \
    SP1_ALPEN_PARAMS_PATH_V1="$fixtures_dir/v1/alpen-params.json" \
      cargo build --release -p strata-sp1-func-test-guests

    export EE_SP1_EE_PARAMS_PATH="$fixtures_dir/ee-params.json"
    export EE_SP1_CHUNK_V0_ELF="$guests_dir/generated/v0/guest-alpen-chunk-v0.elf"
    export EE_SP1_ACCT_V0_ELF="$guests_dir/generated/v0/guest-alpen-acct-v0.elf"
    export EE_SP1_ACCT_V0_PREDICATE_FILE="$guests_dir/generated/v0/acct-predicate.txt"
    export EE_SP1_CHUNK_V1_ELF="$guests_dir/generated/v1/guest-alpen-chunk-v1.elf"
    export EE_SP1_ACCT_V1_ELF="$guests_dir/generated/v1/guest-alpen-acct-v1.elf"
    export EE_SP1_ACCT_V1_PREDICATE_FILE="$guests_dir/generated/v1/acct-predicate.txt"
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
