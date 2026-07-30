"""Real SP1 proof generation check for the EE chunk + acct prover pipeline.

Drives the minimum EVM activity needed to trigger exactly one chunk proof
and one recursive acct proof, then waits on the same service.log signals
as `test_ee_prover_pipeline_alive.py`. Kept minimal (a single transfer, a
one-block chunk/batch) so that under the real SP1 backend this test
produces exactly one proof round instead of the many the "alive" test's
heavier load would trigger.

Backend is controlled by `EE_PROVER_BACKEND` (see
`factories/alpen_client.py`):
  - `native` (default) — zkaleido `NativeHost`. Fast; this test is then
    redundant with (a cheaper version of) the "alive" test.
  - `sp1` — real SP1 Groth16 proving. Requires the guest ELFs built ahead
    of time (`cargo build --release -p strata-sp1-guest-builder`) and the
    SP1 toolchain. Never run in CI (mirrors the `asm` repo's
    `ASM_PROVER_BACKEND` convention) since real local proving can take
    several minutes per proof.

TODO(EE_PROVER_BACKEND=sp1): as written, this currently fails fast under
the real backend. `EeOLEnv` points alpen-client at a real OL node, and
alpen-client validates the OL's registered EE-account `update_vk` against
the SP1-derived predicate key *before* it launches the prover services or
produces a single block (see `launch_validated_ee_batch_prover` in
`bin/alpen-client/src/main.rs`) -- a mismatch there is fatal to the whole
process, not just the final OL-submission step. The OL genesis registers
a fixed native/Schnorr test predicate, so it never matches the real SP1
Groth16 predicate out of the box. Bridging this requires bootstrapping the
OL's `update_vk` to the real SP1 predicate before the sequencer starts:
derive the expected condition (a throwaway `alpen-client --dummy-ol-client`
run under the sp1 backend logs it in its startup-failure message, since
that value depends only on the guest ELF, not on the live OL state), then
register it via the existing admin `PredicateUpdate` mechanism (see
`test_ee_predicate_transition.py` / `common/test_cli.py`'s
`create_ee_predicate_update`) before bringing up the real sequencer env.
Left for follow-up rather than folded into this pass.
"""

import logging
import os

import flexitest

from common.base_test import BaseTest
from common.config.constants import ServiceType
from common.evm import DEV_ACCOUNT_ADDRESS, send_eth_transfer
from common.evm_utils import wait_for_receipt
from common.prover_log_signals import count_log_matches, ee_log_path, wait_for_log_signal
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

TRANSFER_AMOUNT_WEI = 10**16  # 0.01 ETH
TRANSFER_RECIPIENT = "0x000000000000000000000000000000000000dEaD"

# Real local SP1 Groth16 proving (a chunk proof, then a recursive acct proof
# that verifies the chunk proof in-circuit) can take minutes per proof on
# CPU. This timeout is shared with the native backend, where it's a
# harmless upper bound since native proving resolves almost immediately.
SIGNAL_TIMEOUT_SECS = int(os.environ.get("EE_PROVER_SIGNAL_TIMEOUT_SECS", "1800"))


@flexitest.register
class TestEeProverRealProof(BaseTest):
    """Drive exactly one chunk + acct proof round through the EE prover
    pipeline, minimal enough to be practical under real SP1 proving."""

    # Seal a chunk (and batch) after a single block, so one transfer is
    # enough to trigger exactly one proof round.
    BATCH_SEALING_BLOCK_COUNT = 1

    def __init__(self, ctx: flexitest.InitContext):
        # Inline env instance — private log file, same rationale as the
        # "alive" test.
        ctx.set_env(
            EeOLEnv(
                fullnode_count=0,
                pre_generate_blocks=110,
                batch_sealing_block_count=self.BATCH_SEALING_BLOCK_COUNT,
            )
        )

    def main(self, ctx):
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        bitcoin: BitcoinService = self.get_service(ServiceType.Bitcoin)
        rpc = alpen_seq.create_rpc()
        btc_rpc = bitcoin.create_rpc()
        miner_addr = btc_rpc.proxy.getnewaddress()
        log_path = ee_log_path(alpen_seq)
        log_offset = log_path.stat().st_size if log_path.exists() else 0

        nonce = int(rpc.eth_getTransactionCount(DEV_ACCOUNT_ADDRESS, "latest"), 16)
        tx_hash = send_eth_transfer(rpc, nonce, TRANSFER_RECIPIENT, TRANSFER_AMOUNT_WEI)
        receipt = wait_for_receipt(rpc, tx_hash, timeout=120)
        assert receipt["status"] == "0x1", f"transfer failed: {receipt}"
        logger.info(f"transfer accepted at block {int(receipt['blockNumber'], 16)}")

        wait_for_log_signal(
            log_path,
            r"persisted block witness",
            after_offset=log_offset,
            timeout=SIGNAL_TIMEOUT_SECS,
            description="per-block witness persisted at block production",
            btc_rpc=btc_rpc,
            miner_addr=miner_addr,
        )

        wait_for_log_signal(
            log_path,
            r"marking chunk as proof-ready",
            after_offset=log_offset,
            timeout=SIGNAL_TIMEOUT_SECS,
            description="chunk proof completed (ChunkReceiptHook fired)",
            btc_rpc=btc_rpc,
            miner_addr=miner_addr,
        )

        wait_for_log_signal(
            log_path,
            r"persisting batch acct proof",
            after_offset=log_offset,
            timeout=SIGNAL_TIMEOUT_SECS,
            description="acct proof completed (AcctReceiptHook fired)",
            btc_rpc=btc_rpc,
            miner_addr=miner_addr,
        )

        wait_for_log_signal(
            log_path,
            r"submitted snark update to OL",
            after_offset=log_offset,
            timeout=SIGNAL_TIMEOUT_SECS,
            description="acct proof submitted to OL (SnarkAccountUpdate)",
            btc_rpc=btc_rpc,
            miner_addr=miner_addr,
        )

        perm_fail_count = count_log_matches(
            log_path,
            r"retries exhausted|task died mid-Proving and retries exhausted",
            after_offset=log_offset,
        )
        assert perm_fail_count == 0, (
            f"observed {perm_fail_count} permanent prover failure(s) (log: {log_path})"
        )

        logger.info("EE prover pipeline produced a full chunk+acct proof round")
        return True
