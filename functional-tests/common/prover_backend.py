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

from common.config import AlpenProverConfig, AlpenProverProgram
from common.config.params import GenesisAccountData

_REPO_ROOT = Path(__file__).resolve().parents[2]

# Where provers/sp1/build.rs puts the guest ELFs and the acct predicate.
_GENERATED_DIR = _REPO_ROOT / "provers" / "sp1" / "generated"
CHUNK_ELF = _GENERATED_DIR / "guest-alpen-chunk.elf"
ACCT_ELF = _GENERATED_DIR / "guest-alpen-acct.elf"
ACCT_PREDICATE = _GENERATED_DIR / "alpen-acct.predicate"

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

# A second, distinct deterministic acct signing key -- not tied to any
# proof-impl `test_signing_key()`, just a stand-in target predicate for the
# VK-rotation functional test (test_ee_predicate_transition.py). Its chunk
# counterpart is reused from above: a prover program's chunk key only has to
# agree with its own acct key, not with any other program's, so nothing
# requires it to differ.
ROTATED_ACCT_SIGNING_KEY_HEX = "04" * 32

#: (spec_version, chunk_signing_key_hex, acct_signing_key_hex) triples the
#: native backend runs when a test doesn't ask for its own set.
DEFAULT_NATIVE_PROGRAMS: list[tuple[str, str, str]] = [
    ("v0", NATIVE_CHUNK_SIGNING_KEY_HEX, NATIVE_ACCT_SIGNING_KEY_HEX)
]


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
    def prover_config(
        self,
        datadir: Path,
        programs: list[tuple[str, str, str]] | None = None,
    ) -> AlpenProverConfig:
        """Builds the ``[sequencer.prover]`` table, writing any files it needs
        into ``datadir``.

        ``programs`` names one resident spec version per entry, so a test can
        keep two versions' provers live across a VK rotation. Only the native
        backend can honour it."""


@dataclass(frozen=True)
class NativeBackend(ProverBackend):
    """The zkaleido NativeHost: signs proofs rather than proving them."""

    backend: ClassVar[str] = "native"
    genesis_predicate: str = GenesisAccountData().predicate
    ee_params_path: None = None

    def prover_config(
        self,
        datadir: Path,
        programs: list[tuple[str, str, str]] | None = None,
    ) -> AlpenProverConfig:
        entries = []
        for i, (spec_version, chunk_hex, acct_hex) in enumerate(
            programs or DEFAULT_NATIVE_PROGRAMS
        ):
            chunk_key_path = datadir / f"native-chunk-signing-key-{i}.hex"
            acct_key_path = datadir / f"native-acct-signing-key-{i}.hex"
            chunk_key_path.write_text(chunk_hex)
            acct_key_path.write_text(acct_hex)
            entries.append(
                AlpenProverProgram(
                    spec_version=spec_version,
                    chunk_path=str(chunk_key_path),
                    acct_path=str(acct_key_path),
                )
            )
        return AlpenProverConfig(backend=self.backend, programs=entries)


@dataclass(frozen=True)
class Sp1Backend(ProverBackend):
    """The real compiled guest pair from ``provers/sp1``."""

    backend: ClassVar[str] = "sp1"
    genesis_predicate: str
    ee_params_path: Path
    chunk_elf: Path
    acct_elf: Path

    def prover_config(
        self,
        datadir: Path,
        programs: list[tuple[str, str, str]] | None = None,
    ) -> AlpenProverConfig:
        if programs is not None:
            raise ValueError(
                "the sp1 backend ships one guest pair, so per-spec-version programs are native-only"
            )
        return AlpenProverConfig(
            backend=self.backend,
            programs=[
                AlpenProverProgram(
                    spec_version="v0",
                    chunk_path=str(self.chunk_elf),
                    acct_path=str(self.acct_elf),
                )
            ],
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
        )
    raise ValueError(f"Unknown EE_PROVER_BACKEND: {backend!r} (expected: native|sp1)")
