"""Live Alpen EE upgrade: VK rotation activates the next EVM fork.

End-to-end rehearsal of the Alpen upgrade design: the admin rotates the EE
account's update predicate, the rotation lands in the account inbox, and the
sequencer — running an "upgraded" binary whose pending fork (Osaka) is
disabled until the boundary — derives the fork activation from the block
that consumes the VK-update message. No restart happens anywhere.

Asserted end to end:
  1. Pre-boundary the chain runs Prague rules: a transaction above the
     EIP-7825 gas cap (Osaka) is accepted and mined.
  2. The sequencer observes the VK-update message, seals the batch at the
     boundary block, and activates Osaka for the next block (log signal from
     the fork manager).
  3. The boundary batch is proven under the OLD key and accepted by OL — the
     rotation activates on consumption, flipping OL's update_vk to the new
     predicate.
  4. Post-boundary batches are proven under the NEW key and accepted: the
     account's seq_no keeps advancing past the boundary.
  5. Post-boundary the chain runs Osaka rules: a transaction above the
     EIP-7825 cap is rejected, while a normal transaction still lands.
"""

import logging
import re
from pathlib import Path

import flexitest
from eth_account import Account

from common.base_test import BaseTest
from common.config.constants import (
    ALPEN_ACCOUNT_ID,
    DEV_CHAIN_ID,
    DEV_PRIVATE_KEY,
    ServiceType,
)
from common.evm_utils import wait_for_receipt
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.test_cli import create_ee_predicate_update
from common.wait import wait_until, wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

INITIAL_EE_BLOCKS = 5
SIGNAL_TIMEOUT_SECS = 240

# EIP-7825 (Osaka) caps a transaction's gas limit at 2^24 = 16,777,216.
# 20M gas is legal under Prague and illegal under Osaka.
OVER_CAP_GAS = 20_000_000

# Initial Alpen account predicate matches `EeAcctProgram::test_predicate_key()`
# (deterministic test SK = [0x02; 32] in strata_proofimpl_alpen_acct).
INITIAL_ACCT_PREDICATE = (
    "Bip340Schnorr:4d4b6cd1361032ca9bd2aeb9d900aa4d45d9ead80ac9423374c451a7254d0766"
)

# The rotation target matches `EeAcctProgram::test_predicate_key_v2()`
# (deterministic test SK = [0x04; 32]) — the native stand-in for the new
# ELF's VK. Pinned by `v2_predicate_key_matches_pinned_string` in
# `crates/proof-impl/alpen-acct/src/program.rs`.
V2_ACCT_PREDICATE = "Bip340Schnorr:462779ad4aad39514614751a71085f2f10e1c7a593e4e030efb5b8721ce55b0b"

# Emitted by the ForkScheduleManager when the boundary block's derived
# activation is persisted and applied to the live chainspec.
FORK_ACTIVATED_PATTERN = r"activated fork at VK-update boundary"

_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _send_transfer_with_gas(rpc, nonce: int, gas: int) -> str:
    """Sign and broadcast a self-transfer with an explicit gas limit."""
    tx = {
        "nonce": nonce,
        "gasPrice": int(rpc.eth_gasPrice(), 16),
        "gas": gas,
        "to": "0x000000000000000000000000000000000000dEaD",
        "value": 1,
        "data": b"",
        "chainId": DEV_CHAIN_ID,
    }
    signed = Account.sign_transaction(tx, DEV_PRIVATE_KEY)
    return rpc.eth_sendRawTransaction(signed.raw_transaction.hex())


def _log_contains(log_path: Path, pattern: str, after_offset: int) -> bool:
    if not log_path.exists():
        return False
    with log_path.open("rb") as fh:
        fh.seek(after_offset)
        body = fh.read().decode(errors="replace")
    return re.search(pattern, _ANSI_RE.sub("", body)) is not None


@flexitest.register
class TestEeLiveForkUpgrade(BaseTest):
    """Upgrades the EE from Prague to Osaka live, gated by the VK rotation."""

    def __init__(self, ctx: flexitest.InitContext):
        # Private env: we assert on this test's own service log, and the
        # sequencer must run with Osaka pending (disabled until the derived
        # boundary) — the software-rollout half of the upgrade.
        ctx.set_env(
            EeOLEnv(
                fullnode_count=0,
                pre_generate_blocks=110,
                admin_confirmation_depth=2,
                fund_test_cli_wallet=True,
                dev_track_latest_epoch=True,
                batch_sealing_block_count=3,
                pending_evm_forks=["osaka"],
            )
        )

    def main(self, ctx):
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        strata_seq: StrataService = self.get_service(ServiceType.Strata)
        bitcoin: BitcoinService = self.get_service(ServiceType.Bitcoin)

        rpc = alpen_seq.create_rpc()
        btc_rpc = bitcoin.create_rpc()
        strata_rpc = strata_seq.wait_for_rpc_ready(timeout=30)
        strata_seq.wait_for_account_genesis_epoch_commitment(
            ALPEN_ACCOUNT_ID,
            rpc=strata_rpc,
            timeout=30,
        )
        alpen_seq.wait_for_block(INITIAL_EE_BLOCKS, timeout=120)

        mine_addr = btc_rpc.proxy.getnewaddress()
        log_path = Path(alpen_seq.props["datadir"]) / "service.log"

        initial_vk = self._fetch_update_vk(strata_rpc)
        assert initial_vk == INITIAL_ACCT_PREDICATE, (
            f"expected initial update_vk {INITIAL_ACCT_PREDICATE!r}, got {initial_vk!r}"
        )

        # --- 1. Pre-boundary: Prague rules, over-cap gas is legal ---
        nonce = int(rpc.eth_getTransactionCount(Account.from_key(DEV_PRIVATE_KEY).address), 16)
        pre_fork_tx = _send_transfer_with_gas(rpc, nonce, OVER_CAP_GAS)
        nonce += 1
        receipt = wait_for_receipt(rpc, pre_fork_tx, timeout=60)
        assert receipt["status"] == "0x1", f"pre-fork over-cap tx failed: {receipt}"
        logger.info("pre-fork: over-EIP-7825-cap tx mined under Prague rules")

        log_offset = log_path.stat().st_size if log_path.exists() else 0

        # --- 2. Rotate the predicate; the admin update rides L1 into the
        # OL, which appends the VK-update message to the account inbox ---
        admin_xpriv = (Path(strata_seq.props["datadir"]) / "bridge-operator_keys").read_text()
        result = create_ee_predicate_update(
            seq_no=1,
            predicate=V2_ACCT_PREDICATE,
            admin_xpriv=admin_xpriv.strip(),
            btc_url=bitcoin.props["rpc_url"],
            btc_user=bitcoin.props["rpc_user"],
            btc_password=bitcoin.props["rpc_password"],
        )
        logger.info("submitted predicate rotation to %s: %s", V2_ACCT_PREDICATE, result)

        # --- 3. The sequencer derives the boundary and activates Osaka ---
        # Mine while polling: L1 confirmations drive the admin update into
        # the ASM, the OL epoch terminal appends the inbox message, and DA
        # confirmations keep the batch pipeline moving.
        def mine_and_check_fork_activated() -> bool:
            if _log_contains(log_path, FORK_ACTIVATED_PATTERN, log_offset):
                return True
            btc_rpc.proxy.generatetoaddress(2, mine_addr)
            return False

        wait_until(
            mine_and_check_fork_activated,
            error_with="sequencer never derived the fork activation at the VK-update boundary",
            timeout=SIGNAL_TIMEOUT_SECS,
            step=1.0,
        )
        logger.info("sequencer activated Osaka at the derived VK-update boundary")

        # --- 4. The boundary batch (proven under the OLD key) is accepted
        # and its consumption rotates OL's update_vk to the new predicate ---
        def mine_and_fetch_vk() -> str:
            btc_rpc.proxy.generatetoaddress(2, mine_addr)
            return self._fetch_update_vk(strata_rpc)

        wait_until_with_value(
            mine_and_fetch_vk,
            lambda vk: vk == V2_ACCT_PREDICATE,
            error_with="OL update_vk never rotated on consumption of the VK-update message",
            timeout=SIGNAL_TIMEOUT_SECS,
        )
        logger.info("OL rotated update_vk on consumption of the VK-update message")

        boundary_seq_no = self._fetch_seq_no(strata_rpc)

        # --- 5. Post-boundary batches are proven under the NEW key and
        # accepted: seq_no advances past the boundary ---
        def mine_and_fetch_seq_no() -> int:
            btc_rpc.proxy.generatetoaddress(2, mine_addr)
            return self._fetch_seq_no(strata_rpc)

        wait_until_with_value(
            mine_and_fetch_seq_no,
            lambda seq_no: seq_no > boundary_seq_no,
            error_with="no post-boundary batch was accepted under the rotated VK",
            timeout=SIGNAL_TIMEOUT_SECS,
        )
        logger.info("post-boundary batch accepted under the rotated VK")

        # --- 6. Post-boundary: Osaka rules, over-cap gas is now illegal ---
        try:
            over_cap_tx = _send_transfer_with_gas(rpc, nonce, OVER_CAP_GAS)
        except Exception as exc:  # noqa: BLE001 - rejection shape depends on the RPC layer
            logger.info("post-fork: over-cap tx rejected at submission: %s", exc)
        else:
            # Some pool paths accept-then-drop; the tx must never mine.
            try:
                wait_for_receipt(rpc, over_cap_tx, timeout=20)
            except Exception:
                logger.info("post-fork: over-cap tx never mined under Osaka rules")
            else:
                raise AssertionError("over-EIP-7825-cap tx mined after Osaka activation")
            nonce += 1

        normal_tx = _send_transfer_with_gas(rpc, nonce, 21_000)
        receipt = wait_for_receipt(rpc, normal_tx, timeout=60)
        assert receipt["status"] == "0x1", f"post-fork normal tx failed: {receipt}"
        logger.info("post-fork: normal tx mined — chain is healthy under Osaka")

        return True

    @staticmethod
    def _fetch_update_vk(strata_rpc) -> str:
        return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")["update_vk"]

    @staticmethod
    def _fetch_seq_no(strata_rpc) -> int:
        return int(
            strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")["seq_no"]
        )
