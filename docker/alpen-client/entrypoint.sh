#!/bin/sh

# Fail fast on errors and unset variables
set -eu

# Restrict default permissions for newly created files
umask 027

if [ "${1-}" = "help" ] || [ "${1-}" = "--help" ] || [ "${1-}" = "-h" ]; then
    exec alpen-client --help
fi

# Thin launcher. Both Alpen inputs are files the operator supplies and mounts
# into the container: the node's own TOML config (--alpen-config) and the
# chain params artifact (--alpen-params). Neither is built here -- one config
# format, one place it comes from.
#
# Secrets (SEQUENCER_PRIVATE_KEY, STRATA_SUBMIT_RPC_TOKEN) stay in the
# environment; alpen-client reads them straight from there.

ALPEN_CONFIG_PATH="${ALPEN_CONFIG_PATH:-/app/configs/alpen-config.toml}"
ALPEN_PARAMS_PATH="${ALPEN_PARAMS_PATH:-/app/configs/generated/alpen-params.json}"
DATADIR="${DATADIR:-/app/data}"

require_file() {
    if [ ! -f "$2" ]; then
        echo "entrypoint: $1 is \"$2\", which is not a file. Mount it into the container." >&2
        exit 1
    fi
}

require_file ALPEN_CONFIG_PATH "${ALPEN_CONFIG_PATH}"
require_file ALPEN_PARAMS_PATH "${ALPEN_PARAMS_PATH}"

mkdir -p "${DATADIR}"

exec alpen-client \
    --alpen-config "${ALPEN_CONFIG_PATH}" \
    --alpen-params "${ALPEN_PARAMS_PATH}" \
    --datadir "${DATADIR}" \
    --addr 0.0.0.0 \
    --http \
    --http.addr 0.0.0.0 \
    --http.port "${HTTP_PORT:-8545}" \
    --http.api "${HTTP_API:-eth,net,web3,txpool,admin,debug}" \
    --ws \
    --ws.addr 0.0.0.0 \
    --ws.port "${WS_PORT:-8546}" \
    --ws.api "${WS_API:-eth,net,web3,txpool}" \
    --authrpc.addr 0.0.0.0 \
    --authrpc.port "${AUTHRPC_PORT:-8551}" \
    --authrpc.jwtsecret "${JWT_SECRET:-/app/keys/jwt.hex}" \
    --txpool.minimal-protocol-fee "${TXPOOL_MIN_PROTOCOL_FEE:-0}" \
    "$@"
