"""
Waiting utilities for test synchronization.
"""

import logging
import math
import time
from collections.abc import Callable
from typing import Any, TypeVar

from common.config.constants import (
    DEFAULT_BLOCK_WAIT_SLACK_SECONDS,
    DEFAULT_EE_BLOCK_WAIT_SECONDS,
)

from .rpc import RpcError

logger = logging.getLogger(__name__)

# Transient errors that should be retried rather than propagated.
# OSError covers ConnectionError, requests.RequestException (inherits IOError), etc.
_RETRYABLE = (RpcError, OSError)


def timeout_for_expected_blocks(
    expected_blocks: int,
    seconds_per_block: float = DEFAULT_EE_BLOCK_WAIT_SECONDS,
    slack_seconds: int = DEFAULT_BLOCK_WAIT_SLACK_SECONDS,
) -> int:
    """Compute a timeout budget for a block-driven wait."""
    if expected_blocks < 0:
        raise ValueError("expected_blocks must be >= 0")
    if seconds_per_block <= 0:
        raise ValueError("seconds_per_block must be > 0")
    if slack_seconds < 0:
        raise ValueError("slack_seconds must be >= 0")

    return math.ceil(expected_blocks * seconds_per_block + slack_seconds)


def wait_until(
    fn: Callable[[], Any],
    error_with: str = "Timed out",
    timeout: int = 30,
    step: float = 0.5,
):
    """
    Wait until a function call returns truth value, given time step, and timeout.
    This function waits until function call returns truth value at the interval of step seconds.
    """
    deadline = time.monotonic() + timeout

    while True:
        try:
            if fn():
                return
        except _RETRYABLE as e:
            logger.warning(f"caught {type(e).__name__}, will still wait for timeout: {e}")

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break

        time.sleep(min(step, remaining))

    try:
        if fn():
            return
    except _RETRYABLE as e:
        logger.warning(f"caught {type(e).__name__}, will still wait for timeout: {e}")

    raise AssertionError(error_with)


T = TypeVar("T")


def wait_until_with_value(
    fn: Callable[..., T],
    predicate: Callable[[T], bool],
    error_with: str = "Timed out",
    timeout: int = 5,
    step: float = 0.5,
    debug=False,
) -> T:
    """
    Similar to `wait_until` but this returns the value of the function.
    This also takes another predicate which acts on the function value and returns a bool
    """
    deadline = time.monotonic() + timeout

    while True:
        try:
            r = fn()
            if debug:
                print("Waiting.. current value:", r)
            if predicate(r):
                return r
        except _RETRYABLE as e:
            logger.warning(f"caught {type(e).__name__}, will still wait for timeout: {e}")

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break

        time.sleep(min(step, remaining))

    try:
        r = fn()
        if debug:
            print("Waiting.. current value:", r)
        if predicate(r):
            return r
    except _RETRYABLE as e:
        logger.warning(f"caught {type(e).__name__}, will still wait for timeout: {e}")

    raise AssertionError(error_with)


def wait_for_account_update_seq(
    rpc,
    account_id_hex: str,
    min_seq_no: int,
    start_epoch: int,
    btc_rpc,
    miner_addr: str,
    timeout: int = 600,
    blocks_per_step: int = 4,
    poll: float = 1.0,
) -> int:
    """Wait until OL terminal epoch summaries include the submitted update.

    `min_seq_no` is an operation sequence number, matching what epoch
    summaries report under `update_inputs[].seq_no`. Returns the epoch
    whose summary carried the match.
    """
    deadline = time.time() + timeout
    last_terminal_epoch = start_epoch
    last_seen_seq_no = -1
    while time.time() < deadline:
        btc_rpc.proxy.generatetoaddress(blocks_per_step, miner_addr)
        time.sleep(poll)
        status = rpc.strata_getChainStatus()
        latest = status["latest"]
        last_terminal_epoch = int(latest["epoch"])
        for epoch in range(start_epoch, last_terminal_epoch + 1):
            try:
                summary = rpc.strata_getAccountEpochSummary(account_id_hex, epoch)
            except RpcError:
                continue
            updates = (summary.get("update_inputs") or []) if summary else []
            for update in updates:
                seq_no = int(update.get("seq_no", -1))
                last_seen_seq_no = max(last_seen_seq_no, seq_no)
                if seq_no >= min_seq_no:
                    return epoch
    raise AssertionError(
        f"account update seq_no >= {min_seq_no} not found from epoch {start_epoch}; "
        f"last_terminal_epoch={last_terminal_epoch}, last_seen_seq_no={last_seen_seq_no}"
    )
