"""EE rotates to a new key without changing its spec version.

A guest fix that keeps the rules gets a new key, not a new version. The chain
launches on the v2 program, and an admin `PredicateUpdate` names a second v2
program's key. A consumed rotation activates the version of the program its
key belongs to, so the chain stays on V2. The update carrying the rotation is
proved under the old key, and the updates after it under the new one, so the
sequencer has to route each batch by key: both programs prove V2.

Checks that:

- `update_vk` moves to the second program's key,
- a further update settles under that key,
- every block, before and after the rotation, is stamped V2 on both the
  sequencer and the full node, and the two agree on the tip.

The second v2 program only exists under the native prover, since a build
produces one v2 guest pair.
"""

import logging
from pathlib import Path

import flexitest

from common.base_test import BaseTest
from common.config.constants import ALPEN_ACCOUNT_ID, ServiceType
from common.prover_backend import SAME_VERSION_ROTATION_PROGRAMS, NativeBackend
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.test_cli import create_ee_predicate_update
from common.wait import wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

PROVER = NativeBackend(spec_versions=SAME_VERSION_ROTATION_PROGRAMS)

INITIAL_BLOCKS = 5
POST_ADMIN_UPDATE_L1_BLOCKS = 5
BATCH_SEALING_BLOCK_COUNT = 10
PREDICATE_SETTLE_TIMEOUT_SECONDS = 180
POST_ROTATION_UPDATE_TIMEOUT_SECONDS = 240

GENESIS_ACCT_PREDICATE = PROVER.genesis_predicate
REKEYED_ACCT_PREDICATE = PROVER.rotation_target_predicate

EXPECTED_SPEC_VERSION = 2


def _spec_version_of(block: dict) -> int:
    """Decodes the big-endian AlpenSpecId prefix of a block's `extraData`.

    Mirrors `peek_spec_version` in `crates/params/src/extra_data.rs`. V0's
    layout is empty.
    """
    raw = bytes.fromhex(block["extraData"].removeprefix("0x"))
    if not raw:
        return 0
    return int.from_bytes(raw[:2], "big")


def _assert_stamped(rpc, tip: int, node: str) -> None:
    """Checks every block from 1 to `tip` is stamped with the expected version."""
    for height in range(1, tip + 1):
        block = rpc.eth_getBlockByNumber(hex(height), False)
        version = _spec_version_of(block)
        assert version == EXPECTED_SPEC_VERSION, (
            f"{node}: block {height} is stamped v{version}, expected v{EXPECTED_SPEC_VERSION}"
        )


@flexitest.register
class TestEePredicateSameVersion(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            EeOLEnv(
                pre_generate_blocks=110,
                admin_confirmation_depth=2,
                fund_test_cli_wallet=True,
                batch_sealing_block_count=BATCH_SEALING_BLOCK_COUNT,
                # Two v2 programs with different keys. Both are resident from
                # startup, and each batch goes to the one whose key OL will
                # check it against.
                prover=PROVER,
            )
        )

    def main(self, ctx):
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        alpen_fn: AlpenClientService = self.get_service(ServiceType.AlpenFullNode)
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

        admin_key_path = Path(strata_seq.props["datadir"]) / "bridge-operator_keys"
        admin_xpriv = admin_key_path.read_text().strip()
        assert admin_xpriv, f"admin key file is empty: {admin_key_path}"

        mine_addr = btc_rpc.proxy.getnewaddress()

        def fetch_account_and_mine() -> dict:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")

        initial_vk = strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
            "update_vk"
        ]
        assert initial_vk == GENESIS_ACCT_PREDICATE, (
            f"expected initial update_vk {GENESIS_ACCT_PREDICATE!r}, got {initial_vk!r}"
        )

        # --- Rotate to the second v2 program's key ---------------------------
        result = create_ee_predicate_update(
            seq_no=1,
            predicate=REKEYED_ACCT_PREDICATE,
            admin_xpriv=admin_xpriv,
            btc_url=bitcoin.props["rpc_url"],
            btc_user=bitcoin.props["rpc_user"],
            btc_password=bitcoin.props["rpc_password"],
        )
        logger.info("Applied %s update (seq 1): %s", REKEYED_ACCT_PREDICATE, result)
        btc_rpc.proxy.generatetoaddress(POST_ADMIN_UPDATE_L1_BLOCKS, mine_addr)

        rotated = wait_until_with_value(
            fetch_account_and_mine,
            lambda account: account["update_vk"] == REKEYED_ACCT_PREDICATE,
            error_with=f"update_vk did not move to {REKEYED_ACCT_PREDICATE}",
            timeout=PREDICATE_SETTLE_TIMEOUT_SECONDS,
        )
        logger.info("update_vk moved to %s at seq_no %d", REKEYED_ACCT_PREDICATE, rotated["seq_no"])

        # --- A further update settles under the new key ----------------------
        #
        # The update carrying the rotation was checked against the old key.
        # Only a later one shows the sequencer proved past the rotation with
        # the second program, although both programs prove v2.
        wait_until_with_value(
            fetch_account_and_mine,
            lambda account: account["seq_no"] > rotated["seq_no"],
            error_with=(
                f"no update settled after the rotation (seq_no stuck at {rotated['seq_no']}); "
                "the sequencer likely proved it with the old program"
            ),
            timeout=POST_ROTATION_UPDATE_TIMEOUT_SECONDS,
        )
        final_vk = strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
            "update_vk"
        ]
        assert final_vk == REKEYED_ACCT_PREDICATE, (
            f"update_vk should still be {REKEYED_ACCT_PREDICATE!r}, got {final_vk!r}"
        )

        # --- The version never moved -----------------------------------------
        seq_rpc = alpen_seq.create_rpc()
        fn_rpc = alpen_fn.create_rpc()
        tip = int(seq_rpc.eth_blockNumber(), 16)
        alpen_fn.wait_for_block(tip, timeout=60)

        _assert_stamped(seq_rpc, tip, "sequencer")
        _assert_stamped(fn_rpc, tip, "fullnode")
        seq_hash = seq_rpc.eth_getBlockByNumber(hex(tip), False)["hash"]
        fn_hash = fn_rpc.eth_getBlockByNumber(hex(tip), False)["hash"]
        assert seq_hash == fn_hash, f"nodes disagree at block {tip}: {seq_hash} != {fn_hash}"
        logger.info("blocks 1..%d are stamped v%d on both nodes", tip, EXPECTED_SPEC_VERSION)

        return True
