"""Shared EEST environment with backend-aware proof batching."""

import logging

from common.alpen_params import EEST_BASE_FEE_FLOOR
from common.prover_backend import resolve_prover_backend
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

NATIVE_BATCH_SEALING_BLOCK_COUNT = 5
SP1_BATCH_SEALING_BLOCK_COUNT = 100


def create_eest_env() -> EeOLEnv:
    """Resolve the backend once for both proof configuration and batch sizing."""
    prover = resolve_prover_backend()
    # Chunk block count defaults to batch block count in the client. Larger
    # SP1 batches therefore reduce both chunk and account proof frequency.
    # This is not a spending cap; long runs still submit paid proof requests.
    block_count = (
        SP1_BATCH_SEALING_BLOCK_COUNT
        if prover.backend == "sp1"
        else NATIVE_BATCH_SEALING_BLOCK_COUNT
    )
    logger.info("EEST prover=%s batch_sealing_block_count=%d", prover.backend, block_count)
    return EeOLEnv(
        fullnode_count=0,
        pre_generate_blocks=110,
        batch_sealing_block_count=block_count,
        base_fee_floor=EEST_BASE_FEE_FLOOR,
        prover=prover,
    )
