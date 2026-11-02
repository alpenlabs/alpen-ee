"""Lowering the base-fee floor at the V1 -> V2 upgrade.

The V2 floor is a quarter of dev.json's V1 floor. Unlike a raise, the base fee
does not jump: it decays from the V1 floor by EIP-1559's step on an idle chain
until it reaches the V2 floor. The chain must keep building, syncing and
settling.
"""

import logging

import flexitest

from common.base_test import BaseTest
from common.wait import wait_until_with_value
from tests.alpen_client.base_fee_floor.floor_upgrade import (
    BASE_FEE_MAX_CHANGE_DENOMINATOR,
    FLOOR_REACHED_TIMEOUT_SECONDS,
    assert_chain_consistent,
    assert_post_boundary_batch_settles,
    assert_transfer_included,
    base_fee_of,
    dev_base_fee_floor,
    floor_upgrade_env,
    get_block,
    run_floor_upgrade,
)

logger = logging.getLogger(__name__)

V1_FLOOR = dev_base_fee_floor()["v1"]
V2_FLOOR = V1_FLOOR // 4


@flexitest.register
class TestBaseFeeFloorLowerAtUpgrade(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(floor_upgrade_env(V2_FLOOR))

    def main(self, ctx):
        services, boundary = run_floor_upgrade(self, V2_FLOOR)
        fn_rpc = services.fullnode.create_rpc()

        parent_fee = base_fee_of(get_block(fn_rpc, boundary - 1))
        boundary_fee = base_fee_of(get_block(fn_rpc, boundary))
        assert parent_fee == V1_FLOOR
        assert V2_FLOOR < boundary_fee < parent_fee, (
            f"boundary base fee {boundary_fee} should step down from {parent_fee}, "
            f"not jump to the V2 floor {V2_FLOOR}"
        )
        assert boundary_fee >= parent_fee - parent_fee // BASE_FEE_MAX_CHANGE_DENOMINATOR

        floor_height = wait_until_with_value(
            lambda: int(fn_rpc.eth_blockNumber(), 16),
            lambda tip: base_fee_of(get_block(fn_rpc, tip)) == V2_FLOOR,
            error_with=f"base fee never decayed to the V2 floor {V2_FLOOR}",
            timeout=FLOOR_REACHED_TIMEOUT_SECONDS,
        )
        logger.info(
            "base fee decayed %d -> %d over blocks %d..%d",
            V1_FLOOR,
            V2_FLOOR,
            boundary,
            floor_height,
        )

        assert_transfer_included(services, V2_FLOOR)
        assert_chain_consistent(services, {**dev_base_fee_floor(), "v2": V2_FLOOR})
        assert_post_boundary_batch_settles(services)
        return True
