#!/usr/bin/env bash
# Compiles the workload contracts into contracts/out/<Name>.bin (init code as
# hex). The output is committed, so building the generator never needs solc.
set -euo pipefail

SOLC_IMAGE="ethereum/solc:0.8.28"
cd "$(dirname "$0")"

docker run --rm --platform linux/amd64 -v "$PWD:/src" -w /src "$SOLC_IMAGE" \
    --optimize --optimize-runs 200 --evm-version cancun \
    --bin --overwrite -o out Token.sol Pair.sol

# solc also writes the IToken interface, which has no code.
rm -f out/IToken.bin
