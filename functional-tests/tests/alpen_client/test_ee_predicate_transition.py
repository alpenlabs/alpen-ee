"""EE predicate transition functional test.

Verifies the Alpen snark account's `update_vk` rotates via an admin
`PredicateUpdate`, that the sequencer's prover actually switches to the
rotated VK's program for everything proved after the rotation (no restart),
and exercises the boundary of the rotation <-> EE spec-version coupling:
every consumed rotation unconditionally advances to the successor spec
version, and this binary currently only supports spec versions V0 and V1 --
no V2 exists yet. So the first rotation (V0 -> V1) must settle normally, a
further update after it must also settle -- proved under the *new* VK, not
the stale pre-rotation one -- and a second rotation (V1 -> V2) must be
refused rather than silently misapplied.
"""

import logging
import re
from pathlib import Path

import flexitest

from common.base_test import BaseTest
from common.config.constants import ALPEN_ACCOUNT_ID, ServiceType
from common.prover_backend import (
    NATIVE_ACCT_SIGNING_KEY_HEX,
    NATIVE_CHUNK_SIGNING_KEY_HEX,
    ROTATED_ACCT_SIGNING_KEY_HEX,
)
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.test_cli import create_ee_predicate_update
from common.wait import wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

INITIAL_BLOCKS = 5
POST_ADMIN_UPDATE_L1_BLOCKS = 5
PREDICATE_SETTLE_TIMEOUT_SECONDS = 120
POST_ROTATION_UPDATE_TIMEOUT_SECONDS = 180
UNSUPPORTED_ROTATION_TIMEOUT_SECONDS = 120

# Initial Alpen account predicate matches `EeAcctProgram::test_predicate_key()`
# (deterministic test SK = [0x02; 32] in strata_proofimpl_alpen_acct,
# `NATIVE_ACCT_SIGNING_KEY_HEX`).
V0_ACCT_PREDICATE = "Bip340Schnorr:4d4b6cd1361032ca9bd2aeb9d900aa4d45d9ead80ac9423374c451a7254d0766"

# The rotation target: a Bip340Schnorr predicate bound to the deterministic
# SK [0x04; 32] (`ROTATED_ACCT_SIGNING_KEY_HEX`).
V1_ACCT_PREDICATE = "Bip340Schnorr:462779ad4aad39514614751a71085f2f10e1c7a593e4e030efb5b8721ce55b0b"

# A further rotation target, standing in for spec version V2 -- which this
# binary has no support for, so the rotation to it can't be handled and must
# be refused.
V2_ACCOUNT_PREDICATE = "NeverAccept"

UNHONORABLE_ROTATION_LOG_PATTERN = r"consumed a rotation to unknown spec version"

# Service logs include tracing ANSI colour codes even when written to file.
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _ee_log_path(alpen_service: AlpenClientService) -> Path:
    """Path to alpen-client's service log produced by the test harness."""
    return Path(alpen_service.props["datadir"]) / "service.log"


def _count_log_matches(log_path: Path, pattern: str, after_offset: int = 0) -> int:
    """Return the number of `pattern` matches in `log_path` past `after_offset`.

    Tolerates a not-yet-created log file (returns 0).
    """
    if not log_path.exists():
        return 0
    with log_path.open("rb") as fh:
        fh.seek(after_offset)
        body = fh.read().decode(errors="replace")
    body = _ANSI_RE.sub("", body)
    return sum(1 for _ in re.finditer(pattern, body))


@flexitest.register
class TestEePredicateTransition(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            EeOLEnv(
                pre_generate_blocks=110,
                admin_confirmation_depth=2,
                fund_test_cli_wallet=True,
                # Two resident programs, each tagged with the AlpenSpecId
                # it's built for (see ProverProgramPaths in
                # bin/alpen-client/src/config.rs): v1's acct key is the
                # rotation target (see V1_ACCT_PREDICATE above),
                # v0's is the genesis-matching key. Both are validated and
                # loaded at startup; the sequencer routes each batch's proof
                # request to whichever program's declared version matches
                # that batch's own governing spec version (see
                # PaasBatchProver in
                # bin/alpen-client/src/sequencer/prover/batch_prover.rs), so
                # proving keeps working across the V0 -> V1 rotation below
                # without a restart.
                prover_programs=[
                    ("v1", NATIVE_CHUNK_SIGNING_KEY_HEX, ROTATED_ACCT_SIGNING_KEY_HEX),
                    ("v0", NATIVE_CHUNK_SIGNING_KEY_HEX, NATIVE_ACCT_SIGNING_KEY_HEX),
                ],
            )
        )

    def main(self, ctx):
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        strata_seq: StrataService = self.get_service(ServiceType.Strata)
        bitcoin: BitcoinService = self.get_service(ServiceType.Bitcoin)
        btc_rpc = bitcoin.create_rpc()

        strata_rpc = strata_seq.wait_for_rpc_ready(timeout=30)
        strata_seq.wait_for_account_genesis_epoch_commitment(
            ALPEN_ACCOUNT_ID,
            rpc=strata_rpc,
            timeout=30,
        )
        alpen_seq.wait_for_block(INITIAL_BLOCKS, timeout=120)

        # Read the single operator/admin xpriv generated by datatool.
        admin_key_path = Path(strata_seq.props["datadir"]) / "bridge-operator_keys"
        if not admin_key_path.exists():
            raise AssertionError(f"admin key file not found: {admin_key_path}")
        admin_xpriv = admin_key_path.read_text().strip()
        if not admin_xpriv:
            raise AssertionError(f"admin key file is empty: {admin_key_path}")

        btc_url = bitcoin.props["rpc_url"]
        btc_user = bitcoin.props["rpc_user"]
        btc_password = bitcoin.props["rpc_password"]
        mine_addr = btc_rpc.proxy.getnewaddress()

        def fetch_update_vk_and_mine() -> str:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
                "update_vk"
            ]

        initial_vk = strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
            "update_vk"
        ]
        if initial_vk != V0_ACCT_PREDICATE:
            raise AssertionError(
                f"expected initial update_vk to be {V0_ACCT_PREDICATE!r}, got {initial_vk!r}"
            )

        # --- V0 -> V1: the one rotation this binary can honor -----------------
        result = create_ee_predicate_update(
            seq_no=1,
            predicate=V1_ACCT_PREDICATE,
            admin_xpriv=admin_xpriv,
            btc_url=btc_url,
            btc_user=btc_user,
            btc_password=btc_password,
        )
        logger.info("Applied %s update (seq 1): %s", V1_ACCT_PREDICATE, result)
        btc_rpc.proxy.generatetoaddress(POST_ADMIN_UPDATE_L1_BLOCKS, mine_addr)

        wait_until_with_value(
            fetch_update_vk_and_mine,
            lambda vk: vk == V1_ACCT_PREDICATE,
            error_with=f"update_vk did not transition to {V1_ACCT_PREDICATE} in OL state",
            timeout=PREDICATE_SETTLE_TIMEOUT_SECONDS,
        )
        logger.info("update_vk transitioned to %s (V0 -> V1)", V1_ACCT_PREDICATE)

        # --- Post-rotation: a further update must settle under the *new* VK ----
        #
        # The V0 -> V1 transition above only proves that the update carrying
        # the rotation itself settles -- that update's own proof is checked
        # against update_vk as it stood *before* the rotation, so it's
        # provable under the old (v0) program alone. It says nothing about
        # whether the sequencer can keep proving *after* the rotation has
        # landed. Mine enough plain blocks (nothing rotation-specific, so this
        # doesn't depend on per-version guest correctness, only on host-side
        # program routing) to force at least one more ordinary batch through
        # sealing, DA, proving, and OL submission, and confirm the account's
        # update sequence number advances again -- which can only happen if
        # that update's proof verifies against the now-current
        # V1 predicate, i.e. the v1 program.
        seq_no_after_rotation = strata_rpc.strata_getSnarkAccountStateByTag(
            ALPEN_ACCOUNT_ID, "latest"
        )["seq_no"]

        def mine_and_fetch_seq_no() -> int:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")["seq_no"]

        wait_until_with_value(
            mine_and_fetch_seq_no,
            lambda seq_no: seq_no > seq_no_after_rotation,
            error_with=(
                "no further update settled under the rotated VK "
                f"(seq_no stuck at {seq_no_after_rotation}); the sequencer's prover is "
                "likely still proving with the stale, pre-rotation program"
            ),
            timeout=POST_ROTATION_UPDATE_TIMEOUT_SECONDS,
        )
        logger.info(
            "a further update settled under %s (seq_no advanced past %d)",
            V1_ACCT_PREDICATE,
            seq_no_after_rotation,
        )

        # seq_no is what actually proves the further update settled (see
        # above); update_vk itself doesn't move on an ordinary update, only
        # on one that declares a new predicate. Assert it explicitly anyway,
        # as a direct check that the account still sits on V1 and no other
        # rotation slipped in while we were waiting.
        vk_after_further_update = strata_rpc.strata_getSnarkAccountStateByTag(
            ALPEN_ACCOUNT_ID, "latest"
        )["update_vk"]
        assert vk_after_further_update == V1_ACCT_PREDICATE, (
            f"update_vk should still be {V1_ACCT_PREDICATE!r} after the further "
            f"update, got {vk_after_further_update!r}"
        )

        # --- V1 -> V2: no `AlpenSpecId` variant exists for it ------------------
        #
        # The EE sequencer must refuse to consume this rotation rather than
        # build a block under a spec version it doesn't know, so block
        # building stalls with a specific logged error instead.
        log_path = _ee_log_path(alpen_seq)
        log_offset = log_path.stat().st_size if log_path.exists() else 0

        result = create_ee_predicate_update(
            seq_no=2,
            predicate=V2_ACCOUNT_PREDICATE,
            admin_xpriv=admin_xpriv,
            btc_url=btc_url,
            btc_user=btc_user,
            btc_password=btc_password,
        )
        logger.info("Applied %s update (seq 2): %s", V2_ACCOUNT_PREDICATE, result)

        def mine_and_count_refused_rotations() -> int:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            return _count_log_matches(
                log_path, UNHONORABLE_ROTATION_LOG_PATTERN, after_offset=log_offset
            )

        wait_until_with_value(
            mine_and_count_refused_rotations,
            lambda count: count > 0,
            error_with=(
                f"EE sequencer did not refuse the unhonorable V1 -> V2 rotation (log: {log_path})"
            ),
            timeout=UNSUPPORTED_ROTATION_TIMEOUT_SECONDS,
        )
        logger.info("EE sequencer correctly refused the unhonorable V1 -> V2 rotation")

        # The refused rotation was never consumed by a built block, so
        # update_vk must still sit at the last rotation that did settle.
        stalled_vk = strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
            "update_vk"
        ]
        assert stalled_vk == V1_ACCT_PREDICATE, (
            f"update_vk should remain at {V1_ACCT_PREDICATE!r} after the refused "
            f"rotation, got {stalled_vk!r}"
        )

        return True
