"""Raising the base-fee floor at the V1 -> V2 upgrade.

The V2 floor is three times dev.json's V1 floor: well past EIP-1559's 12.5%
per-block step and past the usual 2x wallet headroom. The first V2 block must
jump straight to it, and the chain must keep building, syncing and settling.
"""

import logging

import flexitest

from common.base_test import BaseTest
from tests.alpen_client.base_fee_floor.floor_upgrade import (
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
V2_FLOOR = 3 * V1_FLOOR


@flexitest.register
class TestBaseFeeFloorRaiseAtUpgrade(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(floor_upgrade_env(V2_FLOOR))

    def main(self, ctx):
        services, boundary = run_floor_upgrade(self, V2_FLOOR)
        fn_rpc = services.fullnode.create_rpc()

        # An idle chain sits on the floor, so the jump is exactly V1 -> V2.
        assert base_fee_of(get_block(fn_rpc, boundary - 1)) == V1_FLOOR
        assert base_fee_of(get_block(fn_rpc, boundary)) == V2_FLOOR
        logger.info("base fee jumped %d -> %d at block %d", V1_FLOOR, V2_FLOOR, boundary)

        assert_transfer_included(services, V2_FLOOR)
        assert_chain_consistent(services, {**dev_base_fee_floor(), "v2": V2_FLOOR})
        assert_post_boundary_batch_settles(services)
        return True
