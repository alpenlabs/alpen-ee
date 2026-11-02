"""Shared flow for the base-fee floor upgrade tests.

The chain launches on V1 under the floors of ``params/dev.json``. An admin
predicate update moves it to V2, which carries a different floor. Each block
must then carry exactly ``max(floor(version), eip1559_next(parent))``. The
first V2 block applies the V2 floor against a V1 parent, so a raised floor
lands in one block, and a lowered one is reached by the normal EIP-1559 decay.

The prover is always native. Native proving runs the chunk runtime with the
node's own params, so a settled post-boundary batch shows the proof side
accepts the new floor too. The sp1 guests bake in the params they were built
with, which would not carry this test's V2 floor.
"""

import json
import logging
from dataclasses import dataclass
from pathlib import Path

from common.alpen_params import PARAMS_DIR
from common.config.constants import ALPEN_ACCOUNT_ID, DEV_RECIPIENT_ADDRESS, ServiceType
from common.evm import DEV_ACCOUNT_ADDRESS, send_eth_transfer
from common.evm_utils import wait_for_receipt
from common.prover_backend import ROTATION_SPEC_VERSIONS, NativeBackend
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.test_cli import create_ee_predicate_update
from common.wait import wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

PROVER = NativeBackend(spec_versions=ROTATION_SPEC_VERSIONS)

INITIAL_BLOCKS = 5
POST_ADMIN_UPDATE_L1_BLOCKS = 5
BATCH_SEALING_BLOCK_COUNT = 3
BOUNDARY_TIMEOUT_SECONDS = 180
FLOOR_REACHED_TIMEOUT_SECONDS = 60
SETTLE_TIMEOUT_SECONDS = 180

# Ethereum's EIP-1559 parameters, which the dev chain spec keeps.
BASE_FEE_MAX_CHANGE_DENOMINATOR = 8
ELASTICITY_MULTIPLIER = 2


def dev_base_fee_floor() -> dict[str, int]:
    """Returns the per-version floors of ``params/dev.json``."""
    params = json.loads((PARAMS_DIR / "dev.json").read_text())
    return params["fee_spec"]["base_fee_floor"]


def floor_upgrade_env(v2_floor: int) -> EeOLEnv:
    """Builds the env: dev.json's floors plus ``v2_floor`` for V2."""
    return EeOLEnv(
        pre_generate_blocks=110,
        admin_confirmation_depth=2,
        fund_test_cli_wallet=True,
        epoch_tracking_mode="latest",
        batch_sealing_block_count=BATCH_SEALING_BLOCK_COUNT,
        prover=PROVER,
        base_fee_floor={**dev_base_fee_floor(), "v2": v2_floor},
    )


def spec_version_of(block: dict) -> int:
    """Decodes the big-endian AlpenSpecId prefix of a block's ``extraData``.

    Mirrors `peek_spec_version` in `crates/params/src/extra_data.rs`. V0's
    layout is empty.
    """
    raw = bytes.fromhex(block["extraData"].removeprefix("0x"))
    if not raw:
        return 0
    return int.from_bytes(raw[:2], "big")


def floor_of(floors: dict[str, int], version: int) -> int:
    """Returns the floor of ``version``: its own entry or its predecessor's."""
    for candidate in range(version, -1, -1):
        if f"v{candidate}" in floors:
            return floors[f"v{candidate}"]
    raise AssertionError("floors have no v0 entry")


def eip1559_next(parent: dict) -> int:
    """Returns the plain EIP-1559 base fee of the block after ``parent``."""
    parent_base_fee = int(parent["baseFeePerGas"], 16)
    gas_used = int(parent["gasUsed"], 16)
    gas_target = int(parent["gasLimit"], 16) // ELASTICITY_MULTIPLIER

    if gas_used == gas_target:
        return parent_base_fee
    if gas_used > gas_target:
        delta = parent_base_fee * (gas_used - gas_target) // gas_target
        return parent_base_fee + max(delta // BASE_FEE_MAX_CHANGE_DENOMINATOR, 1)
    delta = parent_base_fee * (gas_target - gas_used) // gas_target
    return parent_base_fee - delta // BASE_FEE_MAX_CHANGE_DENOMINATOR


def base_fee_of(block: dict) -> int:
    return int(block["baseFeePerGas"], 16)


def get_block(rpc, height: int) -> dict:
    return rpc.eth_getBlockByNumber(hex(height), False)


def assert_floored_base_fees(rpc, floors: dict[str, int], tip: int, node: str) -> None:
    """Checks every block from 1 to ``tip`` against the floored rule."""
    parent = get_block(rpc, 0)
    for height in range(1, tip + 1):
        block = get_block(rpc, height)
        expected = max(floor_of(floors, spec_version_of(block)), eip1559_next(parent))
        assert base_fee_of(block) == expected, (
            f"{node}: block {height} (v{spec_version_of(block)}) base fee "
            f"{base_fee_of(block)} != expected {expected}"
        )
        parent = block


@dataclass
class FloorUpgradeServices:
    seq: AlpenClientService
    fullnode: AlpenClientService
    strata: StrataService
    bitcoin: BitcoinService


def run_floor_upgrade(test, v2_floor: int) -> tuple[FloorUpgradeServices, int]:
    """Moves the chain from V1 to V2 and returns the first V2 block's height.

    Before returning, checks that every V1 block so far sat on the V1 floor's
    rule.
    """
    services = FloorUpgradeServices(
        seq=test.get_service(ServiceType.AlpenSequencer),
        fullnode=test.get_service(ServiceType.AlpenFullNode),
        strata=test.get_service(ServiceType.Strata),
        bitcoin=test.get_service(ServiceType.Bitcoin),
    )
    floors = {**dev_base_fee_floor(), "v2": v2_floor}
    btc_rpc = services.bitcoin.create_rpc()
    fn_rpc = services.fullnode.create_rpc()

    strata_rpc = services.strata.wait_for_rpc_ready(timeout=30)
    services.strata.wait_for_account_genesis_epoch_commitment(
        ALPEN_ACCOUNT_ID, rpc=strata_rpc, timeout=30
    )
    services.seq.wait_for_block(INITIAL_BLOCKS, timeout=120)
    services.fullnode.wait_for_block(INITIAL_BLOCKS, timeout=120)

    pre_tip = int(fn_rpc.eth_blockNumber(), 16)
    assert spec_version_of(get_block(fn_rpc, pre_tip)) == 1, "chain should launch on V1"
    assert_floored_base_fees(fn_rpc, floors, pre_tip, "fullnode")
    logger.info("V1 blocks 1..%d follow the V1 floor %d", pre_tip, floors["v1"])

    admin_key_path = Path(services.strata.props["datadir"]) / "bridge-operator_keys"
    admin_xpriv = admin_key_path.read_text().strip()
    assert admin_xpriv, f"admin key file is empty: {admin_key_path}"

    result = create_ee_predicate_update(
        seq_no=1,
        predicate=PROVER.rotation_target_predicate,
        admin_xpriv=admin_xpriv,
        btc_url=services.bitcoin.props["rpc_url"],
        btc_user=services.bitcoin.props["rpc_user"],
        btc_password=services.bitcoin.props["rpc_password"],
    )
    logger.info("Submitted V1 -> V2 predicate update: %s", result)
    mine_addr = btc_rpc.proxy.getnewaddress()
    btc_rpc.proxy.generatetoaddress(POST_ADMIN_UPDATE_L1_BLOCKS, mine_addr)

    # The OL only includes the admin update's manifest once it sees more L1
    # blocks past it, so keep mining while polling.
    def mine_and_find_boundary() -> int | None:
        btc_rpc.proxy.generatetoaddress(1, mine_addr)
        tip = int(fn_rpc.eth_blockNumber(), 16)
        for height in range(pre_tip, tip + 1):
            if spec_version_of(get_block(fn_rpc, height)) == 2:
                return height
        return None

    boundary = wait_until_with_value(
        mine_and_find_boundary,
        lambda height: height is not None,
        error_with="V2 never activated on the fullnode",
        timeout=BOUNDARY_TIMEOUT_SECONDS,
    )
    assert boundary is not None
    logger.info("V2 activates at block %d", boundary)
    return services, boundary


def assert_chain_consistent(services: FloorUpgradeServices, floors: dict[str, int]) -> None:
    """Checks both nodes follow the floored rule and agree on the chain."""
    seq_rpc = services.seq.create_rpc()
    fn_rpc = services.fullnode.create_rpc()
    tip = int(seq_rpc.eth_blockNumber(), 16)
    services.fullnode.wait_for_block(tip, timeout=60)

    assert_floored_base_fees(seq_rpc, floors, tip, "sequencer")
    assert_floored_base_fees(fn_rpc, floors, tip, "fullnode")
    seq_hash = get_block(seq_rpc, tip)["hash"]
    fn_hash = get_block(fn_rpc, tip)["hash"]
    assert seq_hash == fn_hash, f"nodes disagree at block {tip}: {seq_hash} != {fn_hash}"


def assert_transfer_included(services: FloorUpgradeServices, v2_floor: int) -> None:
    """Sends a transfer priced from ``eth_gasPrice`` and checks it lands in V2."""
    seq_rpc = services.seq.create_rpc()
    fn_rpc = services.fullnode.create_rpc()
    nonce = int(seq_rpc.eth_getTransactionCount(DEV_ACCOUNT_ADDRESS, "pending"), 16)
    tx_hash = send_eth_transfer(seq_rpc, nonce, DEV_RECIPIENT_ADDRESS, 1)

    receipt = wait_for_receipt(fn_rpc, tx_hash, timeout=60)
    assert int(receipt["status"], 16) == 1, f"transfer failed: {receipt}"
    block = get_block(fn_rpc, int(receipt["blockNumber"], 16))
    assert spec_version_of(block) == 2, "transfer should land in a V2 block"
    assert int(receipt["effectiveGasPrice"], 16) >= max(v2_floor, base_fee_of(block))


def assert_post_boundary_batch_settles(services: FloorUpgradeServices) -> None:
    """Waits for an update proved after the rotation to settle on OL."""
    btc_rpc = services.bitcoin.create_rpc()
    strata_rpc = services.strata.wait_for_rpc_ready(timeout=30)
    mine_addr = btc_rpc.proxy.getnewaddress()

    def mine_and_fetch_account() -> dict:
        btc_rpc.proxy.generatetoaddress(1, mine_addr)
        return strata_rpc.strata_getSnarkAccountStateByTag(ALPEN_ACCOUNT_ID, "latest")

    rotated = wait_until_with_value(
        mine_and_fetch_account,
        lambda account: account["update_vk"] == PROVER.rotation_target_predicate,
        error_with="update_vk never rotated to the V2 predicate",
        timeout=SETTLE_TIMEOUT_SECONDS,
    )
    wait_until_with_value(
        mine_and_fetch_account,
        lambda account: account["seq_no"] > rotated["seq_no"],
        error_with=(
            f"no update settled after the rotation (seq_no stuck at {rotated['seq_no']}); "
            "the V2 proof likely rejected the floored blocks"
        ),
        timeout=SETTLE_TIMEOUT_SECONDS,
    )
