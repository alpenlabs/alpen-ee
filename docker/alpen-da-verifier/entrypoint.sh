#!/bin/sh
set -eu

umask 027

if [ "${1-}" = "help" ] || [ "${1-}" = "--help" ] || [ "${1-}" = "-h" ]; then
    exec alpen-da-verifier --help
fi

VERIFIER_CONFIG_PATH="${VERIFIER_CONFIG_PATH:-/app/configs/da-verifier.toml}"
ALPEN_PARAMS_PATH="${ALPEN_PARAMS_PATH:-/app/configs/generated/alpen-params.json}"
DATADIR="${DATADIR:-/app/data}"
SNAPSHOT_PATH="${SNAPSHOT_PATH:-${DATADIR}/reconstruction.snapshot}"
GENESIS_L1_HEIGHT="${GENESIS_L1_HEIGHT:-}"

require_file() {
    if [ ! -f "$2" ]; then
        echo "entrypoint: $1 is \"$2\", which is not a file. Mount it into the container." >&2
        exit 1
    fi
}

require_value() {
    if [ -z "$2" ]; then
        echo "entrypoint: $1 must be set." >&2
        exit 1
    fi
}

require_file VERIFIER_CONFIG_PATH "${VERIFIER_CONFIG_PATH}"
require_file ALPEN_PARAMS_PATH "${ALPEN_PARAMS_PATH}"
require_value GENESIS_L1_HEIGHT "${GENESIS_L1_HEIGHT}"
require_value BITCOIND_RPC_USER "${BITCOIND_RPC_USER:-}"
require_value BITCOIND_RPC_PASSWORD "${BITCOIND_RPC_PASSWORD:-}"

case "${GENESIS_L1_HEIGHT}" in
    *[!0-9]*)
        echo "entrypoint: GENESIS_L1_HEIGHT must be a non-negative integer." >&2
        exit 1
        ;;
esac

if [ -e "${SNAPSHOT_PATH}" ] && [ ! -f "${SNAPSHOT_PATH}" ]; then
    echo "entrypoint: SNAPSHOT_PATH is \"${SNAPSHOT_PATH}\", which is not a file." >&2
    exit 1
fi

mkdir -p "${DATADIR}" "$(dirname "${SNAPSHOT_PATH}")"

exec alpen-da-verifier \
    --alpen-params "${ALPEN_PARAMS_PATH}" \
    --config "${VERIFIER_CONFIG_PATH}" \
    --datadir "${DATADIR}" \
    --snapshot "${SNAPSHOT_PATH}" \
    --genesis-l1-height "${GENESIS_L1_HEIGHT}" \
    "$@"
