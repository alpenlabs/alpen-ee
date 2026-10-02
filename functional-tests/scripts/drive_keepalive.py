#!/usr/bin/env python3
"""Drives a ``--keep-alive`` environment so it keeps producing work.

``entry.py --keep-alive`` starts the services and then leaves them alone: it
runs no miner and submits no transactions, because in a normal test run the
test body does both. Left to itself the environment looks healthy — the EE
chain keeps building blocks on its own timer — while bitcoin sits at
``pre_generate_blocks`` forever, so OL epochs never advance and the EE prover
pipeline never receives a batch to prove.

This script supplies the two missing inputs:

* **L1 blocks**, via ``generatetoaddress`` on the environment's bitcoind.
* **EE transactions**, signed with the dev key and submitted to the sequencer.

Both are needed. Mining alone produces empty batches with nothing to prove;
transactions alone pile up on an EE chain whose OL side is frozen. Run it
alongside a keep-alive environment and the prover tables fill.

Usage::

    # terminal 1
    cd functional-tests && ./run_tests.sh --keep-alive el_ol

    # terminal 2
    cd functional-tests && uv run python -m scripts.drive_keepalive

Stop it with Ctrl-C; the environment keeps running.
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import time
import urllib.request
from dataclasses import dataclass

from eth_account import Account

from common.config.constants import DEV_CHAIN_ID, DEV_PRIVATE_KEY

# Defaults match the port ranges `entry.py` hands the service factories: the
# first bitcoind lands on 18443 (RPC on the next port) and the first
# alpen-client on 30303.
DEFAULT_BITCOIN_URL = "http://127.0.0.1:18444"
DEFAULT_BITCOIN_USER = "user"
DEFAULT_BITCOIN_PASSWORD = "password"
DEFAULT_ETH_RPC = "http://127.0.0.1:30303"

# A burn address, so the dev account's balance is the only thing consumed and
# no recipient state accumulates.
BURN_ADDRESS = "0x000000000000000000000000000000000000dEaD"
TRANSFER_WEI = 10**15
GAS_LIMIT = 21_000
MAX_FEE_WEI = 3 * 10**9
PRIORITY_FEE_WEI = 10**9


class RpcError(RuntimeError):
    """A JSON-RPC call returned an error payload."""


@dataclass
class Rpc:
    """A minimal JSON-RPC client, so this script needs no web3 dependency."""

    url: str
    auth: tuple[str, str] | None = None

    def call(self, method: str, params: list | None = None) -> object:
        """Invokes `method` and returns its result, raising on an error reply."""
        body = json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}
        ).encode()
        request = urllib.request.Request(
            self.url, data=body, headers={"Content-Type": "application/json"}
        )
        if self.auth is not None:
            token = base64.b64encode(":".join(self.auth).encode()).decode()
            request.add_header("Authorization", f"Basic {token}")
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.load(response)
        if payload.get("error"):
            raise RpcError(f"{method}: {payload['error']}")
        return payload["result"]


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parses command line arguments."""
    parser = argparse.ArgumentParser(
        prog="drive_keepalive",
        description="Mine L1 blocks and submit EE transactions into a keep-alive env.",
    )
    parser.add_argument(
        "--mine-interval",
        type=float,
        default=3.0,
        help="Seconds between L1 blocks (default: 3).",
    )
    parser.add_argument(
        "--tx-batch",
        type=int,
        default=5,
        help="Transactions submitted per tick, 0 to disable (default: 5).",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=0.0,
        help="Seconds to run; 0 means until interrupted (default: 0).",
    )
    parser.add_argument("--bitcoin-url", default=DEFAULT_BITCOIN_URL)
    parser.add_argument("--bitcoin-user", default=DEFAULT_BITCOIN_USER)
    parser.add_argument("--bitcoin-password", default=DEFAULT_BITCOIN_PASSWORD)
    parser.add_argument("--eth-rpc", default=DEFAULT_ETH_RPC)
    parser.add_argument(
        "--no-mine", action="store_true", help="Submit transactions but do not mine."
    )
    return parser.parse_args(argv[1:])


def main(argv: list[str]) -> int:
    """Drives the environment until the duration elapses or the user interrupts."""
    args = parse_args(argv)

    btc = Rpc(args.bitcoin_url, auth=(args.bitcoin_user, args.bitcoin_password))
    eth = Rpc(args.eth_rpc)
    account = Account.from_key(DEV_PRIVATE_KEY)

    try:
        mining_address = btc.call("getnewaddress")
        start_height = btc.call("getblockcount")
        nonce = int(eth.call("eth_getTransactionCount", [account.address, "pending"]), 16)
    except (OSError, RpcError) as err:
        print(f"cannot reach the environment: {err}", file=sys.stderr)
        print(
            "is `./run_tests.sh --keep-alive <env>` running? "
            "override endpoints with --bitcoin-url / --eth-rpc",
            file=sys.stderr,
        )
        return 1

    print(f"driving env: L1 height {start_height}, dev account {account.address}")
    print(f"mining to {mining_address}" if not args.no_mine else "mining disabled")

    deadline = time.monotonic() + args.duration if args.duration else None
    sent = 0
    ticks = 0

    try:
        while deadline is None or time.monotonic() < deadline:
            if not args.no_mine:
                btc.call("generatetoaddress", [1, mining_address])

            for _ in range(args.tx_batch):
                tx = {
                    "to": BURN_ADDRESS,
                    "value": TRANSFER_WEI,
                    "gas": GAS_LIMIT,
                    "maxFeePerGas": MAX_FEE_WEI,
                    "maxPriorityFeePerGas": PRIORITY_FEE_WEI,
                    "nonce": nonce,
                    "chainId": DEV_CHAIN_ID,
                }
                signed = account.sign_transaction(tx)
                eth.call("eth_sendRawTransaction", ["0x" + signed.raw_transaction.hex()])
                nonce += 1
                sent += 1

            ticks += 1
            if ticks % 10 == 0:
                l1 = btc.call("getblockcount")
                l2 = int(eth.call("eth_blockNumber"), 16)
                print(f"L1 {l1}  EE {l2}  txs sent {sent}", flush=True)

            time.sleep(args.mine_interval)
    except KeyboardInterrupt:
        print("\nstopping; the environment is still running")
    except (OSError, RpcError) as err:
        print(f"stopped: {err}", file=sys.stderr)
        return 1

    print(f"done: {sent} transactions, L1 now {btc.call('getblockcount')}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
