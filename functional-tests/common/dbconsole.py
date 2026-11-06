"""Read-only probes of an EE node's MDBX store through `dbconsole`.

The console attaches read-only, which MDBX allows next to a running node, so
a test can look at the store's tables directly instead of inferring them
from logs or RPC. The binary is on PATH when `run_tests.sh` built it.
"""

import re
import shutil
import subprocess
from pathlib import Path

DBCONSOLE_BIN = "dbconsole"

_MAP_ENTRY = re.compile(r"(\w+): (-?\d+)")


def eval_store(datadir: Path | str, script: str, timeout: int = 60) -> str:
    """Evaluates a rhai `script` against the store under `datadir` and returns
    the console's last output line (the value of the script's last expression).
    """
    binary = shutil.which(DBCONSOLE_BIN)
    if binary is None:
        raise RuntimeError(f"{DBCONSOLE_BIN} is not on PATH; run_tests.sh builds it")
    result = subprocess.run(
        [binary, "--datadir", str(datadir), "--command", script],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"{DBCONSOLE_BIN} failed ({result.returncode}): {result.stderr.strip()}")
    lines = [line for line in result.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"{DBCONSOLE_BIN} printed nothing for: {script}")
    return lines[-1]


def eval_store_ints(datadir: Path | str, script: str, timeout: int = 60) -> dict[str, int]:
    """Like `eval_store` for a script whose last expression is a map of ints,
    e.g. `#{ tip: 12, rows: 3 }`."""
    line = eval_store(datadir, script, timeout=timeout)
    values = {key: int(value) for key, value in _MAP_ENTRY.findall(line)}
    if not values:
        raise RuntimeError(f"expected a map of ints from {DBCONSOLE_BIN}, got: {line!r}")
    return values
