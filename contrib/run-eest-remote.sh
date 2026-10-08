#!/usr/bin/env bash
set -euo pipefail

RPC_ENDPOINT=""
FORK="Osaka"
RPC_CHAIN_ID="2892"
RPC_SEED_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
TX_WAIT_TIMEOUT="120"
EEST_REPO="https://github.com/alpenlabs/execution-spec-tests"
CHECKOUT_DIR="execution-spec-tests"
PYTEST_ARGS_STRING=""

die() {
    echo "$*" >&2
    exit 1
}

while (($#)); do
    case "$1" in
        --rpc-endpoint) RPC_ENDPOINT="${2:?}"; shift 2 ;;
        --fork) FORK="${2:?}"; shift 2 ;;
        --rpc-chain-id) RPC_CHAIN_ID="${2:?}"; shift 2 ;;
        --rpc-seed-key) RPC_SEED_KEY="${2:?}"; shift 2 ;;
        --tx-wait-timeout) TX_WAIT_TIMEOUT="${2:?}"; shift 2 ;;
        --repo) EEST_REPO="${2:?}"; shift 2 ;;
        --checkout-dir) CHECKOUT_DIR="${2:?}"; shift 2 ;;
        --pytest-args) PYTEST_ARGS_STRING="${2:?}"; shift 2 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[[ -n "${RPC_ENDPOINT}" ]] || die "--rpc-endpoint is required"

WORKSPACE_DIR="$(pwd)"
[[ "${CHECKOUT_DIR}" == /* ]] || CHECKOUT_DIR="${WORKSPACE_DIR}/${CHECKOUT_DIR}"

if ! command -v uv >/dev/null 2>&1; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
    export PATH="${HOME}/.local/bin:${PATH}"
fi

[[ -d "${CHECKOUT_DIR}/.git" ]] || git clone "${EEST_REPO}" "${CHECKOUT_DIR}"
cd "${CHECKOUT_DIR}"

uv python install 3.11
uv python pin 3.11
uv sync --all-extras

# skip_tests.yaml in the EEST checkout names tests by their Prague node IDs,
# which carry the hardfork. Rename them to the hardfork under test so the skips
# still match.
# TODO: key skip_tests.yaml in alpenlabs/execution-spec-tests on Osaka, move
# ALPEN_SKIPS into it and drop this rewrite, so all skips live in one place.
SKIP_LIST="alpen_skip_tests.yaml"
if [[ -f skip_tests.yaml ]]; then
    sed "s/fork_Prague-/fork_${FORK}-/g" skip_tests.yaml > "${SKIP_LIST}"
else
    printf 'skip_tests:\n' > "${SKIP_LIST}"
fi

# Skips on top of skip_tests.yaml. An entry matches a node ID exactly or as an
# fnmatch glob. A glob can't contain brackets: fnmatch reads them as a
# character class.
EIP7825="tests/osaka/eip7825_transaction_gas_limit_cap/test_tx_gas_limit.py"
ALPEN_SKIPS=(
    # Alpen/reth treats memory expansion differently on early revert in this edge case.
    "tests/frontier/opcodes/test_call.py::test_call_memory_expands_on_early_revert[fork_${FORK}-state_test]"
    # Execute mode can't send blob (type-3) transactions.
    "tests/osaka/eip7594_peerdas/*.py::*"
    "${EIP7825}::test_transaction_gas_limit_cap[fork_${FORK}-tx_gas_limit_cap_exceeds_maximum1-state_test]"
    "${EIP7825}::test_transaction_gas_limit_cap[fork_${FORK}-tx_gas_limit_cap_over1-state_test]"
    # Execute mode deploys contracts through an initcode prefix capped at 255 bytes,
    # and this test's prefix is longer.
    "${EIP7825}::test_maximum_gas_refund*"
    # These fill a transaction at the 2^24 gas cap with calldata or access
    # lists. The result is over reth's 128 KiB txpool limit on transaction
    # size, which alpen-client can't raise yet (STR-3681).
    "${EIP7825}::test_tx_gas_limit_cap_full_calldata[fork_${FORK}-state_test-zero_byte_True-exceed_tx_gas_limit_False-correct_intrinsic_cost_in_transaction_gas_limit_True]"
    "${EIP7825}::test_tx_gas_limit_cap_full_calldata[fork_${FORK}-state_test-zero_byte_False-exceed_tx_gas_limit_False-correct_intrinsic_cost_in_transaction_gas_limit_True]"
    "${EIP7825}::test_tx_gas_limit_cap_access_list_with_diff_keys[fork_${FORK}-state_test-exceed_tx_gas_limit_False-correct_intrinsic_cost_in_transaction_gas_limit_True]"
    "${EIP7825}::test_tx_gas_limit_cap_access_list_with_diff_addr[fork_${FORK}-state_test-exceed_tx_gas_limit_False-correct_intrinsic_cost_in_transaction_gas_limit_True]"
)
# skip_tests.yaml has no trailing newline, so start on a fresh line.
{
    echo
    printf '  - %s\n' "${ALPEN_SKIPS[@]}"
} >> "${SKIP_LIST}"

PYTEST_ARGS=()
if [[ -n "${PYTEST_ARGS_STRING}" ]]; then
    mapfile -t PYTEST_ARGS < <(python3 -c 'import shlex,sys; print("\n".join(shlex.split(sys.argv[1])))' "${PYTEST_ARGS_STRING}")
fi

uv run --with solc-select solc-select use 0.8.24 --always-install
uv run --with solc-select execute remote \
    -m state_test \
    "--fork=${FORK}" \
    "--rpc-endpoint=${RPC_ENDPOINT}" \
    "--rpc-seed-key=${RPC_SEED_KEY}" \
    "--rpc-chain-id=${RPC_CHAIN_ID}" \
    "--tx-wait-timeout=${TX_WAIT_TIMEOUT}" \
    "--skip-list-file=${SKIP_LIST}" \
    -v \
    "${PYTEST_ARGS[@]}"
