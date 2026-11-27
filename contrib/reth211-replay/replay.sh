#!/usr/bin/env bash
# Run native and SP1 replay for two historical Alpen EE V2 staging blocks.
set -euo pipefail

: "${STAGING_RPC:?Set STAGING_RPC to an Alpen EE staging debug RPC URL}"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
out=${REPLAY_DIR:-$(mktemp -d)}
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$repo"
echo "Replay work directory: $out"

rpc() {
    local key=$1 method=$2 params=$3
    curl --fail-with-body -sS --retry 2 --connect-timeout 15 --max-time 45 \
        -H 'Content-Type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        "$STAGING_RPC" >"$out/$key.json"
    python3 - "$out/$key.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    response = json.load(f)
if response.get("result") is None:
    raise SystemExit(f"{sys.argv[1]}: {response.get('error') or 'null result'}")
PY
}

# Use the params published for this staging deployment rather than inferring
# network identity from a chain ID shared with the local dev config.
curl --fail-with-body -LsS --retry 2 \
    https://github.com/alpenlabs/alpen-ee/releases/download/v0.5.2/staging-alpen-params.json \
    -o "$out/staging-alpen-params.json"
rpc staging-genesis eth_getBlockByNumber '["0x0",false]'

for h in 2264131 2264133; do
    block=$(printf '0x%x' "$h")
    parent=$(printf '0x%x' "$((h - 1))")
    rpc "staging-$h-block" eth_getBlockByNumber "[\"$block\",true]"
    rpc "staging-$h-parent" eth_getBlockByNumber "[\"$parent\",false]"
    rpc "staging-$h-raw" debug_getRawBlock "[\"$block\"]"
    rpc "staging-$h-witness" debug_executionWitness "[\"$block\"]"
    case "$h" in
        2264131) expected=0x50a718d8da7c350fd94a990dc4f8f0b9a1d2708406e523582d653bbc433a0cc2 ;;
        2264133) expected=0x15437b509ab735d8ef3a6085ae699f38d568fa671d8bff81b1aa04a55885be4b ;;
    esac
    cargo run --locked -p alpen-proof-chunk --example replay_rpc_block -- "$out" "$h" "$expected"
done

# Bake staging params into the V2 chunk guest. Restore any previous local
# generated params when done so this check does not change later builds.
generated="$repo/provers/sp1/generated/alpen_params.rs"
mkdir -p "$(dirname "$generated")"
had_generated=0
if test -f "$generated"; then
    had_generated=1
    cp "$generated" "$out/alpen_params.rs.backup"
fi
restore_params() {
    if (( had_generated )); then
        cp "$out/alpen_params.rs.backup" "$generated"
    else
        rm -f "$generated"
    fi
}
trap restore_params EXIT
python3 - "$out/staging-alpen-params.json" "$generated" <<'PY'
import json
import pathlib
import sys

params = json.loads(pathlib.Path(sys.argv[1]).read_text())
value = json.dumps(params, ensure_ascii=True, separators=(",", ":"))
pathlib.Path(sys.argv[2]).write_text("pub const ALPEN_PARAMS_JSON: &str = " + json.dumps(value) + ";\n")
PY
mkdir -p "$out/elf"
(
    cd "$repo/provers/sp1/guest-alpen-chunk"
    cargo prove build --locked --elf-name guest-staging.elf --output-directory "$out/elf"
)
sha256sum "$out/elf/guest-staging.elf"

# The standalone runner only executes the ELF. It never requests proving.
runner="$out/sp1-host"
mkdir -p "$runner/src"
cp "$here/sp1_replay.rs" "$runner/src/main.rs"
cat >"$runner/Cargo.toml" <<'TOML'
[package]
name = "local-sp1-replay"
version = "0.1.0"
edition = "2024"

[dependencies]
sp1-core-executor = "=6.10.0"

[workspace]
TOML
cargo build --manifest-path "$runner/Cargo.toml"
for h in 2264131 2264133; do
    echo "SP1: staging block $h"
    cargo run --manifest-path "$runner/Cargo.toml" -- \
        "$out/elf/guest-staging.elf" \
        "$out/staging-$h-chunk-input.rkyv" \
        "$out/staging-$h-chunk-public.ssz"
done
