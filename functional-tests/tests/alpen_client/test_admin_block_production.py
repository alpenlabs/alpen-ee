"""Test starting and stopping sequencer block production via the admin RPC."""

import logging
import time

import flexitest

from common.base_test import BaseTest
from common.config.constants import DEFAULT_EE_BLOCK_TIME_MS, ServiceType
from common.services.alpen_client import AlpenClientService
from common.wait import wait_until
from envconfigs import AlpenClientEnv

logger = logging.getLogger(__name__)

BLOCK_TIME_S = DEFAULT_EE_BLOCK_TIME_MS / 1000


def assert_height_stays(alpen_seq: AlpenClientService, height: int, blocks: int) -> None:
    """Assert the chain stays at `height` for `blocks` blocktimes."""
    time.sleep(blocks * BLOCK_TIME_S)
    current = alpen_seq.get_block_number()
    assert current == height, f"expected height to stay at {height}, got {current}"


@flexitest.register
class TestAdminBlockProduction(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(AlpenClientEnv(fullnode_count=0))

    def main(self, ctx: flexitest.RunContext) -> bool:
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)

        wait_until(
            lambda: alpen_seq.get_admin_status() is not None,
            error_with="admin RPC did not become ready",
            timeout=30,
        )
        admin = alpen_seq.create_admin_rpc()
        assert alpen_seq.get_admin_status()["block_production_stop_after"] is None
        alpen_seq.wait_for_block(2)

        # Stop after N: produce through N, then hold.
        stop_at = alpen_seq.get_block_number() + 3
        admin.alpenadmin_stopBlockProduction(stop_at)
        assert alpen_seq.get_admin_status()["block_production_stop_after"] == stop_at
        alpen_seq.wait_for_block(stop_at)
        assert_height_stays(alpen_seq, stop_at, blocks=4)
        logger.info("production held at block %d", stop_at)

        admin.alpenadmin_startBlockProduction()
        assert alpen_seq.get_admin_status()["block_production_stop_after"] is None
        alpen_seq.wait_for_block(stop_at + 2)

        # Stop now. Let any in-flight block land before sampling the height.
        admin.alpenadmin_stopBlockProduction()
        assert alpen_seq.get_admin_status()["block_production_stop_after"] == 0
        time.sleep(2 * BLOCK_TIME_S)
        stopped_at = alpen_seq.get_block_number()
        assert_height_stays(alpen_seq, stopped_at, blocks=5)
        logger.info("production stopped at block %d", stopped_at)

        # Idempotent.
        admin.alpenadmin_stopBlockProduction()

        admin.alpenadmin_startBlockProduction()
        alpen_seq.wait_for_block(stopped_at + 2)

        return True
