"""EE live fork upgrade functional test.

Rotates the Alpen EE account's `update_vk` via an Admin subprotocol
transaction (the only on-chain trigger for a spec-version bump -- there is
no separate "activate hardfork" action) and verifies that doing so activates
the Osaka hardfork at the resulting boundary: an OP_CLZ (EIP-7939) probe
transaction must fail before the boundary and succeed after it. Every
assertion is read from independent EE fullnodes, not the sequencer, so the
test also proves fullnodes derive and enforce the boundary on their own
rather than trusting the sequencer's word for it.
"""

import logging
from pathlib import Path

import flexitest
from eth_account import Account
from eth_utils import to_checksum_address

from common.base_test import BaseTest
from common.config.constants import ALPEN_ACCOUNT_ID, DEV_CHAIN_ID, DEV_PRIVATE_KEY, ServiceType
from common.evm import DEV_ACCOUNT_ADDRESS, sign_deploy
from common.evm_utils import wait_for_receipt
from common.prover_backend import ROTATION_SPEC_VERSIONS, ProverBackend, resolve_prover_backend
from common.rpc import RpcError
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.test_cli import create_ee_predicate_update
from common.wait import wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

INITIAL_BLOCKS = 5
POST_ADMIN_UPDATE_L1_BLOCKS = 5
PROBE_CALL_GAS = 100_000

# Real SP1 proving costs minutes per batch where native signing is
# near-instant, so every wait that ends up behind a proof, and how much EE
# activity each batch swallows, is scaled per backend.
NATIVE_BATCH_SEALING_BLOCK_COUNT = 3
SP1_BATCH_SEALING_BLOCK_COUNT = 8
NATIVE_TIMEOUTS = {"spec_version_settle": 180, "update_vk_settle": 300}
SP1_TIMEOUTS = {"spec_version_settle": 900, "update_vk_settle": 2400}


def _batch_sealing_block_count(prover: ProverBackend) -> int:
    """EE blocks per sealed batch for `prover`."""
    if prover.backend == "sp1":
        return SP1_BATCH_SEALING_BLOCK_COUNT
    return NATIVE_BATCH_SEALING_BLOCK_COUNT


def _timeouts(prover: ProverBackend) -> dict[str, int]:
    """Seconds to allow for each wait that sits behind a proof."""
    if prover.backend == "sp1":
        return SP1_TIMEOUTS
    return NATIVE_TIMEOUTS


# --- CLZ probe contract ------------------------------------------------------
#
# Runtime (12 bytes): load 32 bytes of calldata, execute CLZ (opcode 0x1E,
# EIP-7939, Osaka-only), return the result.
#   PUSH1 0x00  CALLDATALOAD  CLZ  PUSH1 0x00  MSTORE  PUSH1 0x20  PUSH1 0x00  RETURN
_CLZ_RUNTIME = bytes.fromhex("6000351e60005260206000f3")
assert len(_CLZ_RUNTIME) == 12

CLZ_PROBE_INPUT = (1).to_bytes(32, "big")
CLZ_PROBE_EXPECTED_OUTPUT = 255  # leading zero bits in a 32-byte encoding of 1


def _clz_probe_init_code() -> bytes:
    """Init code that CODECOPYs `_CLZ_RUNTIME` into the deployed account.

    Same 14-byte CODECOPY-prefix pattern as
    `common.evm.deploy_large_runtime_contract`. Deployment never executes
    CLZ (it only copies+returns the runtime bytes), so it succeeds
    regardless of the active fork -- only a CALL into the deployed contract
    is fork-gated.
    """
    runtime_size = len(_CLZ_RUNTIME)
    prefix_size = 14
    init_code = bytearray()
    init_code += bytes([0x61]) + runtime_size.to_bytes(2, "big")  # PUSH2 <size>
    init_code += bytes([0x60, prefix_size])  # PUSH1 <code offset>
    init_code += bytes([0x60, 0x00])  # PUSH1 0 <dest offset>
    init_code += bytes([0x39])  # CODECOPY
    init_code += bytes([0x61]) + runtime_size.to_bytes(2, "big")  # PUSH2 <size>
    init_code += bytes([0x60, 0x00])  # PUSH1 0
    init_code += bytes([0xF3])  # RETURN
    assert len(init_code) == prefix_size
    init_code += _CLZ_RUNTIME
    return bytes(init_code)


def _sign_clz_call(rpc, *, nonce: int, probe_address: str, gas: int) -> str:
    """Sign a transaction calling the CLZ probe contract. Returns raw tx hex."""
    tx = {
        "nonce": nonce,
        "gasPrice": int(rpc.eth_gasPrice(), 16),
        "gas": gas,
        "to": probe_address,
        "value": 0,
        "data": CLZ_PROBE_INPUT,
        "chainId": DEV_CHAIN_ID,
    }
    signed = Account.sign_transaction(tx, DEV_PRIVATE_KEY)
    return "0x" + signed.raw_transaction.hex()


def _spec_version_from_extra_data(extra_data_hex: str) -> int:
    """Decode the big-endian AlpenSpecId prefix from a block's `extraData`.

    Mirrors `peek_spec_version` in `crates/alpen-ee/params/src/extra_data.rs`.
    """
    hex_body = extra_data_hex[2:] if extra_data_hex.startswith("0x") else extra_data_hex
    raw = bytes.fromhex(hex_body)
    if len(raw) < 2:
        raise ValueError(f"extra_data {extra_data_hex!r} shorter than the spec version prefix")
    return int.from_bytes(raw[:2], "big")


def _eth_call_clz_probe(rpc, probe_address: str, block_number: int) -> bytes:
    """eth_call the CLZ probe pinned to an explicit block number.

    Pinning avoids racing whatever "latest" resolves to -- eth_call is
    evaluated against a specific block's header (which is what picks
    Prague vs. Osaka), it just never gets mined itself.
    """
    result = rpc.eth_call(
        {"to": probe_address, "data": "0x" + CLZ_PROBE_INPUT.hex()},
        hex(block_number),
    )
    return bytes.fromhex(result[2:])


def _status_ok(receipt: dict) -> bool:
    status = receipt["status"]
    return (int(status, 16) if isinstance(status, str) else status) == 1


@flexitest.register
class TestEeLiveForkUpgrade(BaseTest):
    """VK rotation activates Osaka; CLZ fails before, succeeds after -- per fullnodes."""

    def __init__(self, ctx: flexitest.InitContext):
        # Resolved here and read again in `main` (`resolve_prover_backend` is
        # a pure function of EE_PROVER_BACKEND, so both calls agree): the
        # rotation target is whatever predicate the v1 program actually proves
        # under, which differs per backend -- a fixed Schnorr key under
        # native, the freshly built guest's Sp1Groth16 VK under sp1.
        prover = resolve_prover_backend(ROTATION_SPEC_VERSIONS)
        # Inline env rather than a shared named one: this test needs its
        # services configured from the resolved backend, and it mutates the
        # bitcoin chain, so it must not share an env with sibling tests.
        #
        # Two EE fullnodes (not just the sequencer) so a live VK-rotation ->
        # spec-version (hardfork) boundary can be verified as independently
        # observed and enforced by fullnodes, not just claimed by the
        # sequencer. `epoch_tracking_mode="latest"` lets the EE consume the
        # rotation's inbox message without waiting on the L1 checkpoint round
        # trip, and a small `batch_sealing_block_count` keeps the
        # rotation-consuming block's forced batch seal (see
        # alpen-ee-sequencer's force-seal-after-rotation behavior) from
        # stalling the test.
        ctx.set_env(
            EeOLEnv(
                fullnode_count=2,
                pre_generate_blocks=110,
                admin_confirmation_depth=2,
                fund_test_cli_wallet=True,
                epoch_tracking_mode="latest",
                batch_sealing_block_count=_batch_sealing_block_count(prover),
                prover=prover,
            )
        )

    def main(self, ctx):
        prover = resolve_prover_backend(ROTATION_SPEC_VERSIONS)
        rotation_predicate = prover.rotation_target_predicate
        timeouts = _timeouts(prover)

        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        fullnodes: list[AlpenClientService] = [
            self.get_service(f"{ServiceType.AlpenFullNode}_0"),
            self.get_service(f"{ServiceType.AlpenFullNode}_1"),
        ]
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
        for fn in fullnodes:
            fn.wait_for_block(alpen_seq.get_block_number(), timeout=120)

        seq_rpc = alpen_seq.create_rpc()
        fn_rpcs = [fn.create_rpc() for fn in fullnodes]

        # Read the single operator/admin xpriv generated by datatool (same
        # source as test_ee_predicate_transition.py).
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

        # --- Deploy the probe contract (fork-independent step) ---------------
        nonce = int(seq_rpc.eth_getTransactionCount(DEV_ACCOUNT_ADDRESS, "pending"), 16)
        deploy_hash = sign_deploy(seq_rpc, nonce=nonce, data=_clz_probe_init_code(), gas=200_000)
        nonce += 1

        deploy_receipts = [wait_for_receipt(fn_rpc, deploy_hash, timeout=60) for fn_rpc in fn_rpcs]
        for receipt in deploy_receipts:
            assert _status_ok(receipt), (
                f"probe deploy should succeed regardless of fork state, got {receipt}"
            )
        probe_address = to_checksum_address(deploy_receipts[0]["contractAddress"])
        deploy_block = int(deploy_receipts[0]["blockNumber"], 16)
        assert all(int(r["blockNumber"], 16) == deploy_block for r in deploy_receipts), (
            f"fullnodes disagree on the deploy block: {[r['blockNumber'] for r in deploy_receipts]}"
        )
        logger.info("CLZ probe deployed at %s (block %d)", probe_address, deploy_block)

        # --- Before: CLZ must fail pre-Osaka -----------------------------------
        for fn_rpc in fn_rpcs:
            try:
                _eth_call_clz_probe(fn_rpc, probe_address, deploy_block)
            except RpcError:
                pass
            else:
                raise AssertionError("eth_call to CLZ probe should fail before Osaka activates")

        before_hash = seq_rpc.eth_sendRawTransaction(
            _sign_clz_call(seq_rpc, nonce=nonce, probe_address=probe_address, gas=PROBE_CALL_GAS)
        )
        nonce += 1
        for fn_rpc in fn_rpcs:
            receipt = wait_for_receipt(fn_rpc, before_hash, timeout=60)
            assert not _status_ok(receipt), (
                f"CLZ call should fail before Osaka activates, got status {receipt['status']}"
            )
        logger.info("Pre-fork CLZ call failed as expected on both fullnodes")

        # --- Rotate update_vk: the only on-chain trigger for the fork --------
        result = create_ee_predicate_update(
            seq_no=1,
            predicate=rotation_predicate,
            admin_xpriv=admin_xpriv,
            btc_url=btc_url,
            btc_user=btc_user,
            btc_password=btc_password,
        )
        logger.info("Submitted EeStfVk update to %s: %s", rotation_predicate, result)
        btc_rpc.proxy.generatetoaddress(POST_ADMIN_UPDATE_L1_BLOCKS, mine_addr)

        # --- Poll fullnode block headers for the extra_data spec-version -----
        # boundary. This is the real-time signal (see module docstring);
        # OL's `update_vk` is a lagging, post-checkpoint signal checked only
        # as a final sanity check below.
        #
        # The OL sequencer's block-assembly only "buries" (fetches and
        # includes) a confirmed ASM manifest once it observes enough new L1
        # blocks past it -- it does not happen just because the admin update
        # was enacted. So this loop must keep mining L1 blocks on every poll
        # attempt (mirroring test_ee_predicate_transition.py's
        # fetch_update_vk_and_mine), not just once upfront.
        def find_osaka_activation_height(fn_rpc) -> int | None:
            tip = int(fn_rpc.eth_blockNumber(), 16)
            for height in range(deploy_block, tip + 1):
                block = fn_rpc.eth_getBlockByNumber(hex(height), False)
                if _spec_version_from_extra_data(block["extraData"]) == 1:
                    return height
            return None

        def mine_and_find_activation_heights() -> list[int] | None:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            heights = [find_osaka_activation_height(fn_rpc) for fn_rpc in fn_rpcs]
            found = [h for h in heights if h is not None]
            return found if len(found) == len(heights) else None

        activation_heights = wait_until_with_value(
            mine_and_find_activation_heights,
            lambda heights: heights is not None,
            error_with="Osaka spec version (V1) never appeared on all fullnodes",
            timeout=timeouts["spec_version_settle"],
        )
        assert activation_heights is not None
        assert len(set(activation_heights)) == 1, (
            f"fullnodes disagree on the Osaka activation height: {activation_heights}"
        )
        osaka_height = activation_heights[0]
        logger.info("Osaka activates at block %d on both fullnodes", osaka_height)

        for fn_rpc in fn_rpcs:
            before_block = fn_rpc.eth_getBlockByNumber(hex(osaka_height - 1), False)
            after_block = fn_rpc.eth_getBlockByNumber(hex(osaka_height), False)
            assert _spec_version_from_extra_data(before_block["extraData"]) == 0, (
                f"block {osaka_height - 1} should still be spec V0"
            )
            assert _spec_version_from_extra_data(after_block["extraData"]) == 1, (
                f"block {osaka_height} should be spec V1 (Osaka)"
            )

        # --- After: CLZ must succeed post-Osaka, with the right value --------
        for fn_rpc in fn_rpcs:
            output = _eth_call_clz_probe(fn_rpc, probe_address, osaka_height)
            assert int.from_bytes(output, "big") == CLZ_PROBE_EXPECTED_OUTPUT, (
                f"expected CLZ(1) == {CLZ_PROBE_EXPECTED_OUTPUT}, got 0x{output.hex()}"
            )

        after_hash = seq_rpc.eth_sendRawTransaction(
            _sign_clz_call(seq_rpc, nonce=nonce, probe_address=probe_address, gas=PROBE_CALL_GAS)
        )
        nonce += 1
        for fn_rpc in fn_rpcs:
            receipt = wait_for_receipt(fn_rpc, after_hash, timeout=60)
            assert _status_ok(receipt), (
                f"CLZ call should succeed after Osaka activates, got status {receipt['status']}"
            )
        logger.info("Post-fork CLZ call succeeded on both fullnodes")

        # --- Settlement sanity check: OL's update_vk eventually catches up ---
        def fetch_update_vk_and_mine() -> str:
            btc_rpc.proxy.generatetoaddress(1, mine_addr)
            return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")[
                "update_vk"
            ]

        wait_until_with_value(
            fetch_update_vk_and_mine,
            lambda vk: vk == rotation_predicate,
            error_with=f"update_vk did not settle to {rotation_predicate} in OL state",
            timeout=timeouts["update_vk_settle"],
        )
        logger.info("update_vk settled to %s in OL state", rotation_predicate)

        return True
