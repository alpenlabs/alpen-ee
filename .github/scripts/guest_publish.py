#!/usr/bin/env python3
"""guest_publish.py — Helpers for the SP1 guest publish workflows.

Pure stdlib, so it runs on any GitHub-hosted runner without an install step.

Subcommands:
    validate    Check the workflow inputs before any network or build work.
    params      Fetch the network's params JSON from a URL, or copy it from
                params/<network>.json in the repo, to $OUTPUT_PATH.
    stage       Copy the built guests, predicate and params into $DIST_DIR under
                network-prefixed names.

Each subcommand reads its inputs from the environment variables listed on its
function.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import urllib.request
from pathlib import Path
from typing import NoReturn

# Keep in sync with the `network` choice in publish-guests.yml. `workflow_call`
# inputs can't be a choice, so this is the only guard on that path.
NETWORKS = ("dev", "staging", "testnet", "mainnet")

REF_RE = re.compile(r"^[A-Za-z0-9._/@:-]+$")
# github.com /blob/ URLs serve an HTML page, not the raw JSON.
BLOB_URL_RE = re.compile(r"^https://github\.com/[^/]+/[^/]+/blob/")


def fail(message: str) -> NoReturn:
    """Prints a GHA `::error::` annotation and exits non-zero."""
    print(f"::error::{message}", file=sys.stderr)
    sys.exit(1)


def set_outputs(**outputs: str) -> None:
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as f:
        f.writelines(f"{name}={value}\n" for name, value in outputs.items())


def append_summary(lines: list[str]) -> None:
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")


def sha256_hex(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate_network(network: str) -> str:
    if network not in NETWORKS:
        fail(f"network must be one of {', '.join(NETWORKS)} (got {network!r})")
    return network


def published_names(network: str) -> dict[str, str]:
    """Maps each file in provers/sp1/generated/ to its published name.

    The network comes first because a GitHub Release is one flat list of assets
    shared by every network built for a tag, and it lists them by name.
    """
    return {
        "guest-alpen-chunk.elf": f"{network}-guest-alpen-chunk.elf",
        "guest-alpen-acct.elf": f"{network}-guest-alpen-acct.elf",
        "alpen-acct.predicate": f"{network}-alpen-acct.predicate",
    }


def params_name(network: str) -> str:
    return f"{network}-alpen-params.json"


def guest_version(commit: str, params_digest: str) -> str:
    """Names one build. The same commit with different params gives different
    ELFs, so both go in the name."""
    return f"{commit[:8]}-{params_digest[:8]}"


# ---- validate --------------------------------------------------------------


def cmd_validate() -> None:
    """Env: INPUT_NETWORK, INPUT_PARAMS_URL, INPUT_REF (all but the network may
    be empty)."""
    validate_network(os.environ["INPUT_NETWORK"])

    params_url = os.environ.get("INPUT_PARAMS_URL", "")
    if params_url:
        if not params_url.startswith("https://") or re.search(r"\s", params_url):
            fail("params_url must be an https:// URL with no whitespace")
        if BLOB_URL_RE.match(params_url):
            fail(
                "params_url is a github.com /blob/ URL, which serves HTML. "
                "Use the raw.githubusercontent.com URL instead."
            )

    ref = os.environ.get("INPUT_REF", "")
    if ref and not REF_RE.fullmatch(ref):
        fail("ref contains unsupported characters")


# ---- params ----------------------------------------------------------------


def cmd_params() -> None:
    """Env: NETWORK, PARAMS_URL (optional), OUTPUT_PATH, GITHUB_OUTPUT."""
    network = validate_network(os.environ["NETWORK"])
    params_url = os.environ.get("PARAMS_URL", "")
    out_path = Path(os.environ["OUTPUT_PATH"])
    out_path.parent.mkdir(parents=True, exist_ok=True)

    if params_url:
        print(f"fetching {params_url}")
        with urllib.request.urlopen(params_url, timeout=60) as resp:
            out_path.write_bytes(resp.read())
        source = params_url
    else:
        repo_path = Path("params") / f"{network}.json"
        if not repo_path.is_file():
            fail(f"{repo_path} does not exist; add it or pass params_url")
        shutil.copyfile(repo_path, out_path)
        source = str(repo_path)

    # build.rs does the full AlpenParams parse. This only catches an HTML error
    # page or a truncated download before the long build starts.
    try:
        parsed = json.loads(out_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        fail(f"params from {source} are not valid JSON: {e}")
    if not isinstance(parsed, dict):
        fail(f"params from {source} are not a JSON object")

    set_outputs(params_source=source)


# ---- stage -----------------------------------------------------------------


def cmd_stage() -> None:
    """Env: NETWORK, GEN_DIR, PARAMS_PATH, PARAMS_SOURCE, SOURCE_REF, SOURCE_SHA,
    DIST_DIR, GITHUB_OUTPUT, GITHUB_STEP_SUMMARY."""
    network = validate_network(os.environ["NETWORK"])
    gen_dir = Path(os.environ["GEN_DIR"])
    params_path = Path(os.environ["PARAMS_PATH"])
    params_source = os.environ["PARAMS_SOURCE"]
    source_ref = os.environ["SOURCE_REF"]
    source_sha = os.environ["SOURCE_SHA"]
    dist_dir = Path(os.environ["DIST_DIR"])
    dist_dir.mkdir(parents=True, exist_ok=True)

    names = published_names(network)
    for src_name, dst_name in names.items():
        src = gen_dir / src_name
        if not src.is_file() or src.stat().st_size == 0:
            fail(f"expected build output missing or empty: {src}")
        shutil.copyfile(src, dist_dir / dst_name)
    shutil.copyfile(params_path, dist_dir / params_name(network))

    published = [*names.values(), params_name(network)]
    digests = {name: sha256_hex(dist_dir / name) for name in published}
    version = guest_version(source_sha, digests[params_name(network)])

    set_outputs(version=version)
    append_summary(
        [
            f"## SP1 guests: `{network}`",
            "",
            f"- source: `{source_ref}` @ `{source_sha}`",
            f"- params: `{params_source}`",
            f"- version: `{version}`",
            "",
            "| File | sha256 |",
            "|------|--------|",
            *(f"| `{name}` | `{digest}` |" for name, digest in digests.items()),
            "",
            (
                "To check these files, rebuild at the same commit with `--features docker-build`"
                f" and `SP1_ALPEN_PARAMS_PATH` pointing at `{params_name(network)}`."
            ),
            "",
        ]
    )


# ---- entry point -----------------------------------------------------------

COMMANDS = {
    "validate": cmd_validate,
    "params": cmd_params,
    "stage": cmd_stage,
}


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Helpers for the SP1 guest publish workflows."
    )
    parser.add_argument("command", choices=sorted(COMMANDS))
    COMMANDS[parser.parse_args().command]()


if __name__ == "__main__":
    main()
