#!/usr/bin/env python3
"""guest_publish.py — Helpers for the SP1 guest publish workflows.

Pure stdlib, so it runs on any GitHub-hosted runner without an install step.

Subcommands:
    validate    Check the workflow inputs before any network or build work.
    params      Fetch the network's params JSON from a URL, or copy it from
                params/<network>.json in the repo, to $OUTPUT_PATH.
    stage       Copy the built guests, predicate and params into $DIST_DIR under
                network-prefixed names.
    notes       Put each network's account predicate and the rebuild command at
                the top of the draft release's notes.
    fetch       Download a published release's guest files into
                $OUTPUT_DIR/<network>/, check each against the release
                attestation and write a manifest.json per network.
    upload      Copy each network's files to
                s3://<bucket>/<prefix>/<network>/<version>/, each with a
                `<name>.sha256` sidecar, then manifest.json last as the
                completion marker.

Each subcommand reads its inputs from the environment variables listed on its
function.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path
from typing import NoReturn

# Keep in sync with the `network` choice in publish-guests.yml. `workflow_call`
# inputs can't be a choice, so this is the only guard on that path.
NETWORKS = ("dev", "staging", "testnet", "mainnet")

REF_RE = re.compile(r"^[A-Za-z0-9._/@:-]+$")
TAG_RE = re.compile(r"^[A-Za-z0-9._-]{1,200}$")
SHA1_RE = re.compile(r"^[0-9a-f]{40}$")
VERSION_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{8}$")
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


def gh(*args: str) -> str:
    """Runs the GitHub CLI and returns stdout. `gh` reads GH_TOKEN itself."""
    return subprocess.run(
        ["gh", *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def aws(*args: str) -> str:
    """Runs the AWS CLI and returns stdout. check=True so an AWS error fails the
    step instead of reading as an empty result."""
    return subprocess.run(
        ["aws", *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


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


def release_files(network: str) -> list[str]:
    return [*published_names(network).values(), params_name(network)]


def guest_version(commit: str, params_digest: str) -> str:
    """Names one build. The same commit with different params gives different
    ELFs, so both go in the name."""
    return f"{commit[:8]}-{params_digest[:8]}"


# ---- validate --------------------------------------------------------------


def cmd_validate() -> None:
    """Env: INPUT_NETWORK, INPUT_PARAMS_URL, INPUT_REF, INPUT_RELEASE_TAG (all but
    the network may be empty)."""
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

    release_tag = os.environ.get("INPUT_RELEASE_TAG", "")
    if release_tag and not TAG_RE.fullmatch(release_tag):
        fail("release_tag contains unsupported characters (allowed: [A-Za-z0-9._-])")


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


# ---- notes -----------------------------------------------------------------

PREDICATE_SUFFIX = "-alpen-acct.predicate"
# Wrap the guests section so a rerun replaces it instead of adding a second copy.
NOTES_START = "<!-- sp1-guests -->"
NOTES_END = "<!-- /sp1-guests -->"


def cmd_notes() -> None:
    """Env: TAG, WORK_DIR, GH_TOKEN, GITHUB_REPOSITORY.

    The predicates go in the notes so they can be read without downloading
    anything.
    """
    tag = os.environ["TAG"]
    repo = os.environ["GITHUB_REPOSITORY"]
    work_dir = Path(os.environ["WORK_DIR"])
    work_dir.mkdir(parents=True, exist_ok=True)

    gh(
        "release",
        "download",
        tag,
        "--repo",
        repo,
        "--dir",
        str(work_dir),
        "--pattern",
        f"*{PREDICATE_SUFFIX}",
    )
    predicates = {
        validate_network(path.name.removesuffix(PREDICATE_SUFFIX)): path.read_text(
            encoding="utf-8"
        ).strip()
        for path in sorted(work_dir.glob(f"*{PREDICATE_SUFFIX}"))
    }
    if not predicates:
        fail(f"release {tag} has no *{PREDICATE_SUFFIX} assets")

    lines = [
        NOTES_START,
        "## SP1 guests",
        "",
        (
            "Each network's guests bake in its `<network>-alpen-params.json`. To check a"
            " network's files, rebuild at this tag with those params and compare digests:"
        ),
        "",
        "```sh",
        "SP1_ALPEN_PARAMS_PATH=/abs/path/to/<network>-alpen-params.json \\",
        "  cargo build --release --locked -p alpen-sp1-guest-builder --features docker-build",
        "```",
        "",
        "The build writes the ELFs and the account predicate to `provers/sp1/generated/`.",
        "",
    ]
    for network, predicate in predicates.items():
        lines += [f"### `{network}` account predicate", "", "```", predicate, "```", ""]
    lines.append(NOTES_END)

    body = gh("release", "view", tag, "--repo", repo, "--json", "body", "--jq", ".body")
    if NOTES_END in body:
        body = body.split(NOTES_END, 1)[1]
    notes = work_dir / "notes.md"
    notes.write_text("\n".join(lines) + "\n\n" + body.lstrip("\n"), encoding="utf-8")
    gh("release", "edit", tag, "--repo", repo, "--notes-file", str(notes))


# ---- fetch -----------------------------------------------------------------


def cmd_fetch() -> None:
    """Env: TAG, OUTPUT_DIR, GH_TOKEN, GITHUB_REPOSITORY, GITHUB_STEP_SUMMARY."""
    tag = os.environ["TAG"]
    if not TAG_RE.fullmatch(tag):
        fail("tag contains unsupported characters (allowed: [A-Za-z0-9._-])")
    repo = os.environ["GITHUB_REPOSITORY"]
    output_dir = Path(os.environ["OUTPUT_DIR"])
    download_dir = output_dir / "release"

    gh("release", "download", tag, "--repo", repo, "--dir", str(download_dir))
    networks = [n for n in NETWORKS if (download_dir / params_name(n)).is_file()]
    if not networks:
        fail(f"release {tag} has no <network>-alpen-params.json assets")
    found = sorted(path.name for path in download_dir.iterdir())
    expected = sorted(name for network in networks for name in release_files(network))
    if found != expected:
        fail(f"release {tag} has {found}, expected {expected}")
    for name in found:
        # Checks the file's digest against the attestation GitHub signed when
        # the release was published.
        gh("release", "verify-asset", tag, str(download_dir / name), "--repo", repo)

    # The commits endpoint resolves both lightweight and annotated tags.
    commit = gh("api", f"repos/{repo}/commits/{tag}", "--jq", ".sha").strip()
    if not SHA1_RE.fullmatch(commit):
        fail(f"could not resolve {repo} tag {tag} to a commit sha (got {commit!r})")

    release_url = f"https://github.com/{repo}/releases/tag/{tag}"
    summary = [
        "## SP1 guests to S3",
        "",
        f"- release: [`{tag}`]({release_url}) @ `{commit}`",
        "",
    ]
    for network in networks:
        net_dir = output_dir / network
        net_dir.mkdir(parents=True, exist_ok=True)
        for name in release_files(network):
            shutil.move(download_dir / name, net_dir / name)
        digests = {name: sha256_hex(net_dir / name) for name in release_files(network)}
        predicate_name = published_names(network)["alpen-acct.predicate"]
        manifest = {
            "tag": tag,
            "commit": commit,
            "network": network,
            "version": guest_version(commit, digests[params_name(network)]),
            "release_url": release_url,
            "alpen_acct_predicate": (net_dir / predicate_name).read_text().strip(),
            "sha256": digests,
        }
        (net_dir / "manifest.json").write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        summary += [
            f"### `{network}`: `{manifest['version']}`",
            "",
            "```",
            *(f"{digest}  {name}" for name, digest in digests.items()),
            "```",
            "",
        ]
    append_summary(summary)


# ---- upload ----------------------------------------------------------------


def s3_cp(src: Path, dst: str) -> None:
    if not src.is_file() or src.stat().st_size == 0:
        fail(f"expected upload file missing or empty: {src}")
    print(f"uploading {src} -> {dst}")
    aws("s3", "cp", "--no-progress", str(src), dst)


def first_key(bucket: str, prefix: str) -> str | None:
    key = aws(
        "s3api",
        "list-objects-v2",
        "--bucket",
        bucket,
        "--prefix",
        prefix,
        "--max-items",
        "1",
        "--query",
        "Contents[0].Key",
        "--output",
        "text",
    ).strip()
    return None if key in ("", "None") else key  # an empty listing prints "None"


def published_manifest(bucket: str, key: str) -> dict | None:
    """Returns the manifest already at `key`, or None if there is none."""
    # Compare exactly so a longer key sharing the prefix (e.g. manifest.json.bak)
    # doesn't count as published.
    if first_key(bucket, key) != key:
        return None
    try:
        return json.loads(aws("s3", "cp", f"s3://{bucket}/{key}", "-"))
    except json.JSONDecodeError as e:
        fail(f"s3://{bucket}/{key} is not valid JSON ({e})")


def upload_network(net_dir: Path, bucket: str, prefix: str) -> list[str]:
    """Uploads one network's files with manifest.json last. A present manifest
    means a completed publish: matching digests are a no-op (an rc and its final
    release tagged on one commit), differing digests fail. Objects without a
    manifest are an interrupted publish and are uploaded again."""
    manifest = json.loads((net_dir / "manifest.json").read_text())
    network = validate_network(manifest["network"])
    version = manifest["version"]
    if not VERSION_RE.fullmatch(version):
        fail(f"manifest version is not S3-key-safe: {version!r}")
    key_base = f"{prefix}/{network}/{version}"
    base = f"s3://{bucket}/{key_base}"

    existing = published_manifest(bucket, f"{key_base}/manifest.json")
    if existing is not None:
        if existing.get("sha256") != manifest["sha256"]:
            fail(
                f"{base}/ was published from {existing.get('tag')!r} with different "
                f"digests than {manifest['tag']!r}: two builds of {version} differ"
            )
        print(f"{base}/ already published from {existing.get('tag')}, digests match")
        return []
    if first_key(bucket, f"{key_base}/"):
        print(f"::warning::{base}/ has objects but no manifest.json, uploading again")

    uris = []
    for name, digest in manifest["sha256"].items():
        s3_cp(net_dir / name, f"{base}/{name}")
        # `sha256sum -c` format, like asm and strata-bridge.
        sidecar = net_dir / f"{name}.sha256"
        sidecar.write_text(f"{digest}  {name}\n", encoding="utf-8")
        s3_cp(sidecar, f"{base}/{name}.sha256")
        uris += [f"{base}/{name}", f"{base}/{name}.sha256"]
    s3_cp(net_dir / "manifest.json", f"{base}/manifest.json")
    return [*uris, f"{base}/manifest.json"]


def cmd_upload() -> None:
    """Env: OUTPUT_DIR, S3_BUCKET, S3_PREFIX, GITHUB_STEP_SUMMARY."""
    output_dir = Path(os.environ["OUTPUT_DIR"])
    bucket = os.environ["S3_BUCKET"]
    prefix = os.environ["S3_PREFIX"]

    summary = ["### S3 upload", ""]
    for manifest_path in sorted(output_dir.glob("*/manifest.json")):
        uris = upload_network(manifest_path.parent, bucket, prefix)
        if uris:
            summary += [f"- `{uri}`" for uri in uris]
        else:
            summary.append(f"- `{manifest_path.parent.name}`: already published")
    append_summary([*summary, ""])


# ---- entry point -----------------------------------------------------------

COMMANDS = {
    "validate": cmd_validate,
    "params": cmd_params,
    "stage": cmd_stage,
    "notes": cmd_notes,
    "fetch": cmd_fetch,
    "upload": cmd_upload,
}


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Helpers for the SP1 guest publish workflows."
    )
    parser.add_argument("command", choices=sorted(COMMANDS))
    COMMANDS[parser.parse_args().command]()


if __name__ == "__main__":
    main()
