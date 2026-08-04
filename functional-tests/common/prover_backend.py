"""Resolves which EE prover backend a functional test runs against.

Mirrors the sibling ``asm`` repo's ``ASM_PROVER_BACKEND`` convention
(``asm/functional-tests/run_test.sh`` + ``envs/prover_env.py``): the backend
is picked once, from ``EE_PROVER_BACKEND``, matching whatever
``run_tests.sh`` actually built --

- ``native`` (default): debug build, the EE chunk/acct "provers" just sign
  with a fixed deterministic Schnorr test key instead of doing real ZK
  proving.
- ``sp1``: release build, two real compiled SP1 guest program pairs (``v0``,
  ``v1``) built by ``provers/sp1-func-test-guests`` -- see that crate's
  ``build.rs`` and ``functional-tests/scripts/gen_sp1_guest_params.py`` for
  how their artifacts are produced. Real SP1 proving is unusably slow in
  debug, same reasoning as ``asm``.
"""

import os
from dataclasses import dataclass
from pathlib import Path

from factories.alpen_client import (
    V0_ACCT_PREDICATE,
    V0_ACCT_SIGNING_KEY_HEX,
    V0_NATIVE_CHUNK_SIGNING_KEY_HEX,
    V1_ACCT_PREDICATE,
    V1_ACCT_SIGNING_KEY_HEX,
)

# Env vars run_tests.sh exports when EE_PROVER_BACKEND=sp1, pointing at the
# artifacts provers/sp1-func-test-guests/build.rs produced.
_ENV_CHUNK_V0_ELF = "EE_SP1_CHUNK_V0_ELF"
_ENV_ACCT_V0_ELF = "EE_SP1_ACCT_V0_ELF"
_ENV_ACCT_V0_PREDICATE_FILE = "EE_SP1_ACCT_V0_PREDICATE_FILE"
_ENV_CHUNK_V1_ELF = "EE_SP1_CHUNK_V1_ELF"
_ENV_ACCT_V1_ELF = "EE_SP1_ACCT_V1_ELF"
_ENV_ACCT_V1_PREDICATE_FILE = "EE_SP1_ACCT_V1_PREDICATE_FILE"
_ENV_EE_PARAMS_PATH = "EE_SP1_EE_PARAMS_PATH"


@dataclass
class ProverBackendChoice:
    """Everything a test needs to run against the resolved prover backend."""

    backend: str
    """``"native"`` or ``"sp1"`` -- passed straight through to
    ``AlpenClientFactory.create_sequencer``'s ``prover_backend``."""

    prover_programs: list[tuple[str, str, str]]
    """``(spec_version, chunk_path_or_key, acct_path_or_key)`` candidates --
    signing-key hex under native, real ELF paths under sp1."""

    v0_acct_predicate: str
    v1_acct_predicate: str

    genesis_predicate_override: str | None
    """Passed to ``StrataEnvConfig``/``EeOLEnv``'s ``alpen_predicate_override``.
    ``None`` under native: ``datatool gen-ol-params --alpen-predicate
    bip340-schnorr-test`` already produces a matching genesis predicate. Set
    to the real v0 predicate under sp1, since datatool has no way to derive
    that value itself (see module docs on the sp1-groth16 dead end)."""

    ee_params_path_override: Path | None
    """Passed to ``StrataEnvConfig``/``EeOLEnv``'s ``ee_params_path_override``.
    ``None`` under native: the harness's own freshly-generated ee-params.json
    is fine, nothing else needs to agree with it. Set under sp1 so the live
    EE node's ``alpen-params.json`` is composed from the exact same
    ee-params.json baked into the v0/v1 guest ELFs, rather than relying on
    both sides independently regenerating identical content."""


def _resolve_native() -> ProverBackendChoice:
    return ProverBackendChoice(
        backend="native",
        prover_programs=[
            ("v1", V0_NATIVE_CHUNK_SIGNING_KEY_HEX, V1_ACCT_SIGNING_KEY_HEX),
            ("v0", V0_NATIVE_CHUNK_SIGNING_KEY_HEX, V0_ACCT_SIGNING_KEY_HEX),
        ],
        v0_acct_predicate=V0_ACCT_PREDICATE,
        v1_acct_predicate=V1_ACCT_PREDICATE,
        genesis_predicate_override=None,
        ee_params_path_override=None,
    )


def _require_env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(
            f"{name} is not set -- EE_PROVER_BACKEND=sp1 requires run_tests.sh to have built the "
            "v0/v1 SP1 guest pair first (see functional-tests/run_tests.sh's build_sp1_guests)"
        )
    return value


def _resolve_sp1() -> ProverBackendChoice:
    chunk_v0 = _require_env(_ENV_CHUNK_V0_ELF)
    acct_v0 = _require_env(_ENV_ACCT_V0_ELF)
    chunk_v1 = _require_env(_ENV_CHUNK_V1_ELF)
    acct_v1 = _require_env(_ENV_ACCT_V1_ELF)
    v0_predicate = Path(_require_env(_ENV_ACCT_V0_PREDICATE_FILE)).read_text().strip()
    v1_predicate = Path(_require_env(_ENV_ACCT_V1_PREDICATE_FILE)).read_text().strip()
    ee_params_path = Path(_require_env(_ENV_EE_PARAMS_PATH))

    return ProverBackendChoice(
        backend="sp1",
        prover_programs=[
            ("v1", chunk_v1, acct_v1),
            ("v0", chunk_v0, acct_v0),
        ],
        v0_acct_predicate=v0_predicate,
        v1_acct_predicate=v1_predicate,
        genesis_predicate_override=v0_predicate,
        ee_params_path_override=ee_params_path,
    )


def resolve_prover_backend() -> ProverBackendChoice:
    """Resolves the prover backend from ``EE_PROVER_BACKEND`` (default: native)."""
    backend = os.environ.get("EE_PROVER_BACKEND", "native")
    if backend == "native":
        return _resolve_native()
    if backend == "sp1":
        return _resolve_sp1()
    raise ValueError(f"Unknown EE_PROVER_BACKEND: {backend!r} (expected: native|sp1)")
