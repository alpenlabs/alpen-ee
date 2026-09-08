"""Resolves which EE prover backend a functional test runs against.

Mirrors the sibling ``asm`` repo's ``ASM_PROVER_BACKEND`` convention: the
backend is picked once, from ``EE_PROVER_BACKEND``, matching whatever
``run_tests.sh`` actually built.

- ``native`` (default): the chunk/acct provers sign with a fixed test key
  instead of proving anything.
- ``sp1``: the real guest pair from ``provers/sp1``. Needs a release build --
  SP1 proving is unusably slow in debug.

The sp1 artifact paths are fixed by where ``provers/sp1/build.rs`` and
``scripts/gen_sp1_guest_params.py`` write them, so they are derived here
rather than passed in.
"""

import os
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar

from common.config import AlpenProverConfig
from common.config.params import GenesisAccountData

_REPO_ROOT = Path(__file__).resolve().parents[2]

# Where provers/sp1/build.rs puts the guest ELFs and the acct predicate.
_GENERATED_DIR = _REPO_ROOT / "provers" / "sp1" / "generated"
CHUNK_ELF = _GENERATED_DIR / "guest-alpen-chunk.elf"
ACCT_ELF = _GENERATED_DIR / "guest-alpen-acct.elf"
ACCT_PREDICATE = _GENERATED_DIR / "alpen-acct.predicate"
SP1_PROOF_DEADLINE_ENV = "ALPEN_SP1_PROOF_DEADLINE_SECS"

# Where scripts/gen_sp1_guest_params.py puts the params the guests bake in.
GUEST_PARAMS_DIR = _REPO_ROOT / "target" / "sp1-guest-params"
EE_PARAMS = GUEST_PARAMS_DIR / "ee-params.json"
ALPEN_PARAMS = GUEST_PARAMS_DIR / "alpen-params.json"

# The native provers sign proofs with these Schnorr keys instead of doing real
# ZK proving -- see `EeAcctProgram::test_signing_key` /
# `EeChunkProgram::test_signing_key` in crates/proof-impl/alpen-{acct,chunk}.
# Signing with a different key still produces a validly-signed proof, but OL
# checks it against the genesis predicate and rejects it, so the two have to be
# chosen together: that is why `NativeBackend` owns both halves. These are not
# secrets (unlike SEQUENCER_PRIVATE_KEY), just a fixed publicly-known value.
NATIVE_CHUNK_SIGNING_KEY_HEX = "03" * 32
NATIVE_ACCT_SIGNING_KEY_HEX = "02" * 32


class ProverBackend(ABC):
    """How a test's EE provers run, and what OL genesis must expect of them."""

    backend: ClassVar[str]

    genesis_predicate: str
    """Predicate OL genesis registers for the EE account. Has to match what
    this backend's acct proofs are actually signed or proved with."""

    ee_params_path: Path | None
    """ee-params.json the node must reuse rather than generate, for backends
    pinned to one. Only sp1 is."""

    @abstractmethod
    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        """Builds the ``[sequencer.prover]`` table, writing any files it needs
        into ``datadir``."""


@dataclass(frozen=True)
class NativeBackend(ProverBackend):
    """The zkaleido NativeHost: signs proofs rather than proving them."""

    backend: ClassVar[str] = "native"
    genesis_predicate: str = GenesisAccountData().predicate
    ee_params_path: None = None

    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        chunk_key_path = datadir / "native-chunk-signing-key.hex"
        acct_key_path = datadir / "native-acct-signing-key.hex"
        chunk_key_path.write_text(NATIVE_CHUNK_SIGNING_KEY_HEX)
        acct_key_path.write_text(NATIVE_ACCT_SIGNING_KEY_HEX)
        return AlpenProverConfig(
            backend=self.backend,
            chunk_signing_key_path=str(chunk_key_path),
            acct_signing_key_path=str(acct_key_path),
        )


@dataclass(frozen=True)
class Sp1Backend(ProverBackend):
    """The real compiled guest pair from ``provers/sp1``."""

    backend: ClassVar[str] = "sp1"
    genesis_predicate: str
    ee_params_path: Path
    chunk_elf: Path
    acct_elf: Path
    deadline_secs: int | None = None

    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        return AlpenProverConfig(
            backend=self.backend,
            chunk_elf_path=str(self.chunk_elf),
            acct_elf_path=str(self.acct_elf),
            deadline_secs=self.deadline_secs,
        )


#: Shared default for the many call sites that never override the backend.
NATIVE_BACKEND = NativeBackend()


def _require_built(path: Path) -> Path:
    if not path.exists():
        raise RuntimeError(
            f"{path} is missing -- EE_PROVER_BACKEND=sp1 requires run_tests.sh to have built "
            "the SP1 guest pair first (see its build_sp1_guests)"
        )
    return path


def _optional_positive_int_env(name: str) -> int | None:
    raw_value = os.environ.get(name)
    if raw_value is None:
        return None
    try:
        value = int(raw_value)
    except ValueError as exc:
        raise ValueError(f"{name} must be a positive integer, got {raw_value!r}") from exc
    if value <= 0:
        raise ValueError(f"{name} must be a positive integer, got {value}")
    return value


def resolve_prover_backend() -> ProverBackend:
    """Resolves the prover backend from ``EE_PROVER_BACKEND`` (default: native)."""
    backend = os.environ.get("EE_PROVER_BACKEND", "native")
    if backend == "native":
        return NATIVE_BACKEND
    if backend == "sp1":
        return Sp1Backend(
            genesis_predicate=_require_built(ACCT_PREDICATE).read_text().strip(),
            ee_params_path=_require_built(EE_PARAMS),
            chunk_elf=_require_built(CHUNK_ELF),
            acct_elf=_require_built(ACCT_ELF),
            deadline_secs=_optional_positive_int_env(SP1_PROOF_DEADLINE_ENV),
        )
    raise ValueError(f"Unknown EE_PROVER_BACKEND: {backend!r} (expected: native|sp1)")
