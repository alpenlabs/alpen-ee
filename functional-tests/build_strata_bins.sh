#!/bin/bash
# Builds the strata binaries required by the functional tests from the strata
# git revision pinned in the root Cargo.toml, so factories find them on PATH
# without this workspace building strata itself.
#
# Prints the directory containing the built binaries on stdout (all other
# output goes to stderr), so callers can do:
#
#   export PATH="$(./build_strata_bins.sh):$PATH"
#
# Environment:
#   STRATA_SRC_DIR  checkout/build dir (default: <repo>/target/strata-git)
#   CARGO_RELEASE   set to 1 to build in release mode (mirrors run_tests.sh)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_MANIFEST="$ROOT_DIR/Cargo.toml"

# The pinned strata git dependency in the root Cargo.toml is the single source
# of truth for the strata revision.
STRATA_GIT_URL="$(sed -n 's/^strata-primitives = { git = "\([^"]*\)".*/\1/p' "$ROOT_MANIFEST" | head -n1)"
STRATA_GIT_REV="$(sed -n 's/^strata-primitives = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$ROOT_MANIFEST" | head -n1)"

if [ -z "$STRATA_GIT_URL" ] || [ -z "$STRATA_GIT_REV" ]; then
    echo "error: could not extract strata git url/rev from $ROOT_MANIFEST" >&2
    exit 1
fi

SRC_DIR="${STRATA_SRC_DIR:-$ROOT_DIR/target/strata-git}"

echo "strata source: $STRATA_GIT_URL @ $STRATA_GIT_REV" >&2
echo "checkout dir:  $SRC_DIR" >&2

# Never let git discovery climb out of the checkout. CI's Rust cache saves
# `target/` with every file stripped and the directories kept, so a restored
# checkout is a skeleton whose `.git` is not a repository; without a ceiling,
# every `git -C "$SRC_DIR"` below would silently act on this workspace's own
# repository instead (rewriting its remote and checking strata out over it).
export GIT_CEILING_DIRECTORIES="$ROOT_DIR"

mkdir -p "$SRC_DIR"
SRC_DIR="$(cd "$SRC_DIR" && pwd -P)"
if [ -e "$SRC_DIR/.git" ] \
    && [ "$(git -C "$SRC_DIR" rev-parse --show-toplevel 2>/dev/null || true)" != "$SRC_DIR" ]; then
    echo "stale checkout at $SRC_DIR (not a repository); recreating it" >&2
    rm -rf "$SRC_DIR"
    mkdir -p "$SRC_DIR"
fi
if [ ! -e "$SRC_DIR/.git" ]; then
    git init -q "$SRC_DIR"
    git -C "$SRC_DIR" remote add origin "$STRATA_GIT_URL"
fi
git -C "$SRC_DIR" remote set-url origin "$STRATA_GIT_URL"

# Fetch the pinned rev only if it isn't present already (keeps CI cache warm).
if ! git -C "$SRC_DIR" rev-parse --quiet --verify "$STRATA_GIT_REV^{commit}" >/dev/null; then
    git -C "$SRC_DIR" fetch --depth 1 origin "$STRATA_GIT_REV" >&2 \
        || git -C "$SRC_DIR" fetch origin "$STRATA_GIT_REV" >&2
fi
git -C "$SRC_DIR" checkout -q --detach "$STRATA_GIT_REV"
if [ ! -f "$SRC_DIR/Cargo.toml" ]; then
    echo "error: $SRC_DIR holds no Cargo.toml after checking out $STRATA_GIT_REV" >&2
    exit 1
fi

PROFILE_DIR="debug"
PROFILE_ARGS=""
if [ "${CARGO_RELEASE:-}" = 1 ]; then
    PROFILE_DIR="release"
    PROFILE_ARGS="--release"
fi

# alpen-client is not built here; it comes from this workspace. Keep the strata
# build in its own target dir so it never clashes with the workspace target dir.
# shellcheck disable=SC2086 # PROFILE_ARGS is intentionally word-split
CARGO_TARGET_DIR="$SRC_DIR/target" cargo build \
    --manifest-path "$SRC_DIR/Cargo.toml" \
    --locked \
    $PROFILE_ARGS \
    -F sequencer -F debug-utils -F prover \
    --bin strata \
    --bin strata-signer \
    --bin strata-datatool \
    --bin strata-test-cli \
    >&2

echo "$SRC_DIR/target/$PROFILE_DIR"
