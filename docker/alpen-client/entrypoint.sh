#!/bin/sh

# Fail fast on errors and unset variables
set -eu

# Restrict default permissions for newly created files
umask 027

if [ "${1-}" = "help" ] || [ "${1-}" = "--help" ] || [ "${1-}" = "-h" ]; then
    exec alpen-client --help
fi

# Generate this node's own TOML config (--alpen-config) from environment
# variables, then hand off to alpen-client alongside reth's own flags.
# Set SEQUENCER_MODE=true to run as sequencer (default: fullnode).
#
# Secrets (SEQUENCER_PRIVATE_KEY, STRATA_SUBMIT_RPC_TOKEN) are deliberately
# never written into the generated file -- alpen-client reads them straight
# from the environment.

SEQUENCER_MODE="${SEQUENCER_MODE:-false}"
SEQUENCER_PUBKEY="${SEQUENCER_PUBKEY:?SEQUENCER_PUBKEY must be set}"
ALPEN_PARAMS_PATH="${ALPEN_PARAMS_PATH:-/app/configs/generated/alpen-params.json}"
DATADIR="${DATADIR:-/app/data}"
CONFIG_PATH="${DATADIR}/alpen-config.toml"

mkdir -p "${DATADIR}"

{
    echo "l1_reorg_safe_depth = ${L1_REORG_SAFE_DEPTH:-4}"
    echo "genesis_l1_height = ${GENESIS_L1_HEIGHT:?GENESIS_L1_HEIGHT must be set}"

    if [ "${SEQUENCER_MODE}" = "true" ]; then
        echo 'mode = "sequencer"'
    else
        echo 'mode = "full_node"'
    fi

    echo "[ol]"
    if [ "${DUMMY_OL_CLIENT:-0}" = "1" ]; then
        echo 'source = "dummy"'
    else
        echo 'source = "rpc"'
        echo "client_url = \"${OL_CLIENT_URL:-ws://strata:8432}\""
        if [ "${SEQUENCER_MODE}" = "true" ]; then
            echo "submit_url = \"${OL_SUBMIT_URL:-ws://strata:8435}\""
            # The bearer token authenticating submission is a secret, read
            # directly from the environment by alpen-client -- not written
            # here -- but only actually required for a real (non-dummy) OL
            # connection, so only check for it in that case.
            : "${STRATA_SUBMIT_RPC_TOKEN:?STRATA_SUBMIT_RPC_TOKEN must be set}"
        fi
    fi

    if [ "${SEQUENCER_MODE}" != "true" ]; then
        echo "[full_node]"
        echo "sequencer_pubkey = \"${SEQUENCER_PUBKEY}\""
        # Only needed to forward user-submitted transactions to the sequencer's
        # mempool; omit to run as a read-only full node.
        if [ -n "${SEQUENCER_HTTP_URL:-}" ]; then
            echo "sequencer_http_url = \"${SEQUENCER_HTTP_URL}\""
        fi
    else
        BITCOIND_RPC_URL="${BITCOIND_RPC_URL:?BITCOIND_RPC_URL must be set}"
        BITCOIND_RPC_USER="${BITCOIND_RPC_USER:?BITCOIND_RPC_USER must be set}"
        BITCOIND_RPC_PASSWORD="${BITCOIND_RPC_PASSWORD:?BITCOIND_RPC_PASSWORD must be set}"
        BTCIO_FEE_POLICY="${BTCIO_FEE_POLICY:-bitcoind}"

        echo "[sequencer]"
        echo "batch_sealing_block_count = ${BATCH_SEALING_BLOCK_COUNT:-120}"
        if [ "${DEV_NATIVE_PROVER:-0}" = "1" ]; then
            echo "dev_native_prover = true"
        fi

        echo "[sequencer.bitcoind]"
        echo "rpc_url = \"${BITCOIND_RPC_URL}\""
        echo "rpc_user = \"${BITCOIND_RPC_USER}\""
        echo "rpc_password = \"${BITCOIND_RPC_PASSWORD}\""
        echo "network = \"${BITCOIN_NETWORK:-regtest}\""

        echo "[sequencer.l1_fee_policy]"
        echo "fee_policy = \"${BTCIO_FEE_POLICY}\""
        case "${BTCIO_FEE_POLICY}" in
        bitcoind)
            if [ -n "${BTCIO_CONF_TARGET:-}" ]; then
                echo "bitcoind_conf_target = ${BTCIO_CONF_TARGET}"
            fi
            ;;
        fixed)
            echo "fixed_fee_rate = ${BTCIO_FEE_RATE:?BTCIO_FEE_RATE must be set when BTCIO_FEE_POLICY=fixed}"
            ;;
        mempool)
            echo "mempool_base_url = \"${BTCIO_MEMPOOL_BASE_URL:?BTCIO_MEMPOOL_BASE_URL must be set when BTCIO_FEE_POLICY=mempool}\""
            if [ -n "${BTCIO_MEMPOOL_TIER:-}" ]; then
                echo "mempool_fee_policy = \"${BTCIO_MEMPOOL_TIER}\""
            fi
            if [ -n "${BTCIO_CONF_TARGET:-}" ]; then
                echo "mempool_fallback_conf_target = ${BTCIO_CONF_TARGET}"
            fi
            ;;
        esac
    fi
} >"${CONFIG_PATH}"

exec alpen-client \
    --alpen-config "${CONFIG_PATH}" \
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
