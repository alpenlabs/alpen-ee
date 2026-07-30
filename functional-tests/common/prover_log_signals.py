"""
Log-signal polling helpers for the EE chunk/acct prover pipeline.

The prover pipeline (chunk seal -> chunk proof -> acct proof -> OL
submission) doesn't yet expose a proper RPC/state accessor for "did this
stage complete", so tests observe it through `service.log` lines. This is
shared by both the native (mock) and real-SP1-backend prover pipeline
tests; only the timeouts a caller picks should differ between them.

N.B. Intended to be short-lived and replaced by proper accessors or state
asserts once the pipeline exposes them (see `test_ee_prover_pipeline_alive.py`).
"""

import logging
import re
from pathlib import Path

from common.services.alpen_client import AlpenClientService
from common.wait import wait_until_with_value

logger = logging.getLogger(__name__)

# Service logs include tracing ANSI colour codes even when written to file.
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def ee_log_path(alpen_service: AlpenClientService) -> Path:
    """Path to alpen-client's service log produced by the test harness."""
    return Path(alpen_service.props["datadir"]) / "service.log"


def count_log_matches(log_path: Path, pattern: str, after_offset: int = 0) -> int:
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


def wait_for_log_signal(
    log_path: Path,
    pattern: str,
    after_offset: int,
    timeout: int,
    description: str,
    btc_rpc,
    miner_addr: str,
    btc_blocks_per_step: int = 4,
    poll: float = 1.0,
) -> int:
    """Poll until at least one match for `pattern` appears past `after_offset`.

    Mines bitcoin blocks between polls so the batch DA confirmations
    advance, which is what eventually drives the batch lifecycle into
    `ProofPending` and triggers the chunk + acct prover request.
    """

    def mine_and_count() -> int:
        count = count_log_matches(log_path, pattern, after_offset)
        if count == 0:
            btc_rpc.proxy.generatetoaddress(btc_blocks_per_step, miner_addr)
        return count

    count = wait_until_with_value(
        mine_and_count,
        lambda c: c > 0,
        error_with=(
            f"{description}: no log match for {pattern!r} within {timeout}s (log: {log_path})"
        ),
        timeout=timeout,
        step=poll,
    )
    logger.info(f"{description}: observed {count} match(es)")
    return count
