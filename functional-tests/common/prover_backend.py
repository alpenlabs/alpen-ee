"""Resolves which EE prover backend a functional test runs against.

Mirrors the sibling ``asm`` repo's ``ASM_PROVER_BACKEND`` convention: the
backend is picked once, from ``EE_PROVER_BACKEND``, matching whatever
``run_tests.sh`` actually built.

- ``native`` (default): the chunk/acct provers sign with a fixed test key
  instead of proving anything.
- ``sp1``: the real guest pair from ``provers/sp1``. Needs a release build --
  SP1 proving is unusably slow in debug.

A backend is resolved for a set of spec versions, and owns the whole mapping
from version to program. Tests that never cross a VK rotation take the
default single-``v1`` set: v1 is what the current source builds, and v0 is
only the program already deployed on live networks. A rotation test takes
``ROTATION_SPEC_VERSIONS`` instead, which starts the chain back on v0 so the
rotation has somewhere to rotate from. Which program a version maps to is the
backend's business, not the test's, so the spec schedule the chain launches
on, the predicate OL registers at genesis, and the predicate a rotation
targets always agree with the programs actually configured.

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

_REPO_ROOT = Path(__file__).resolve().parents[2]

# Where provers/sp1/build.rs puts the guest ELFs and the acct predicate. This
# is the build of the *current* workspace source, which is the v1 program:
# both guests bake in `AlpenSpecId::V1`.
_GENERATED_DIR = _REPO_ROOT / "provers" / "sp1" / "generated"
CHUNK_ELF = _GENERATED_DIR / "guest-alpen-chunk.elf"
ACCT_ELF = _GENERATED_DIR / "guest-alpen-acct.elf"
ACCT_PREDICATE = _GENERATED_DIR / "alpen-acct.predicate"

# Prebuilt v0 guest pair, committed as test data: the current workspace
# cannot build a pre-Osaka guest, so a rotation test needs one from
# elsewhere. See that directory's README for provenance.
_SP1_V0_TESTDATA_DIR = _REPO_ROOT / "functional-tests" / "testdata" / "sp1" / "v0"
V0_CHUNK_ELF = _SP1_V0_TESTDATA_DIR / "guest-alpen-chunk.elf"
V0_ACCT_ELF = _SP1_V0_TESTDATA_DIR / "guest-alpen-acct.elf"
V0_ACCT_PREDICATE = _SP1_V0_TESTDATA_DIR / "alpen-acct.predicate"

# Where scripts/gen_sp1_guest_params.py puts the params the guests bake in.
GUEST_PARAMS_DIR = _REPO_ROOT / "target" / "sp1-guest-params"
EE_PARAMS = GUEST_PARAMS_DIR / "ee-params.json"
ALPEN_PARAMS = GUEST_PARAMS_DIR / "alpen-params.json"

#: Every spec version this binary knows, oldest first.
SPEC_VERSIONS = ("v0", "v1")

#: The spec versions a test that never crosses a rotation needs a program
#: for. A fresh chain launches on the version the current source builds, so
#: v0 -- the program already deployed on live networks -- only shows up where
#: a test rotates away from it.
DEFAULT_SPEC_VERSIONS = ("v1",)

#: The spec versions a VK-rotation test needs resident at once: the genesis
#: program, plus the successor the rotation activates. Such a test launches
#: the chain a version back, on v0, so that v1 is still ahead of it.
ROTATION_SPEC_VERSIONS = ("v0", "v1")

# The native provers sign proofs with these Schnorr keys instead of doing real
# ZK proving. Signing with a different key still produces a validly-signed
# proof, but OL checks it against the registered predicate and rejects it, so
# a version's key and the predicate registered for it have to be chosen
# together: that is why `NativeBackend` owns both halves. These are not
# secrets (unlike SEQUENCER_PRIVATE_KEY), just fixed publicly-known values.
#
# v1 runs `EeAcctProgram::test_signing_key` / `EeChunkProgram::test_signing_key`
# from crates/proof-impl/alpen-{acct,chunk}. It has to: the dummy OL client
# reports `EeAcctProgram::test_predicate_key()` as the expected update_vk, and
# the sequencer refuses to start unless a resident program matches it. So the
# default program is the one an env with no real OL already expects. v0's key
# is just a second deterministic value -- nothing in the crates points at it,
# and native proving never verifies anything, so it only has to agree with the
# predicate registered for v0.
#
# One chunk key serves both versions: a program's chunk key only has to agree
# with its own acct key, not with any other program's.
NATIVE_CHUNK_SIGNING_KEY_HEX = "03" * 32
NATIVE_V0_ACCT_SIGNING_KEY_HEX = "04" * 32
NATIVE_V1_ACCT_SIGNING_KEY_HEX = "02" * 32

# The `Bip340Schnorr` predicate each acct key above is bound to. Pinned rather
# than derived: deriving one here would need a BIP340 implementation in the
# harness, and the point of a fixture is that it doesn't move.
NATIVE_V0_ACCT_PREDICATE = (
    "Bip340Schnorr:462779ad4aad39514614751a71085f2f10e1c7a593e4e030efb5b8721ce55b0b"
)
NATIVE_V1_ACCT_PREDICATE = (
    "Bip340Schnorr:4d4b6cd1361032ca9bd2aeb9d900aa4d45d9ead80ac9423374c451a7254d0766"
)

#: Signing-key pair the native backend runs for each spec version.
_NATIVE_SIGNING_KEYS: dict[str, tuple[str, str]] = {
    "v0": (NATIVE_CHUNK_SIGNING_KEY_HEX, NATIVE_V0_ACCT_SIGNING_KEY_HEX),
    "v1": (NATIVE_CHUNK_SIGNING_KEY_HEX, NATIVE_V1_ACCT_SIGNING_KEY_HEX),
}

#: Predicate each native program's proofs are signed under.
_NATIVE_PREDICATES: dict[str, str] = {
    "v0": NATIVE_V0_ACCT_PREDICATE,
    "v1": NATIVE_V1_ACCT_PREDICATE,
}


class ProverBackend(ABC):
    """How a test's EE provers run, and what OL genesis must expect of them."""

    backend: ClassVar[str]

    spec_versions: tuple[str, ...]
    """Spec versions this backend has a resident program for. The first is the
    genesis version; a rotation targets the one after it."""

    genesis_predicate: str
    """Predicate OL genesis registers for the EE account. Has to match what
    this backend's ``spec_versions[0]`` program's proofs are actually signed
    or proved with."""

    ee_params_path: Path | None
    """ee-params.json the node must reuse rather than generate, for backends
    pinned to one. Only sp1 is."""

    @abstractmethod
    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        """Builds the ``[sequencer.prover]`` table, writing any files it needs
        into ``datadir``, with one
        ``[sequencer.prover.programs.<spec_version>]`` entry per
        [`spec_versions`]."""

    @property
    @abstractmethod
    def rotation_target_predicate(self) -> str:
        """Predicate of the successor version's program -- what an admin
        `PredicateUpdate` must rotate to for proving to survive the boundary.

        Only meaningful when the backend was resolved for more than one spec
        version.
        """

    @property
    def genesis_spec_schedule(self) -> dict[str, int]:
        """``spec_schedule`` the chain's params must carry.

        Every version up to and including the genesis version activates at
        coordinate 0; whatever a rotation is still expected to activate stays
        unscheduled. So the chain launches on ``spec_versions[0]``, and the
        batches it stamps name a version this backend has a program for.
        """
        genesis_version = self.spec_versions[0]
        launched = SPEC_VERSIONS[: SPEC_VERSIONS.index(genesis_version) + 1]
        return {spec_version: 0 for spec_version in launched}

    def _require_rotation_versions(self) -> None:
        if len(self.spec_versions) < 2:
            raise ValueError(
                f"{type(self).__name__} was resolved for {self.spec_versions}, so it has no "
                "rotation target; resolve it with ROTATION_SPEC_VERSIONS instead"
            )


@dataclass(frozen=True)
class NativeBackend(ProverBackend):
    """The zkaleido NativeHost: signs proofs rather than proving them."""

    backend: ClassVar[str] = "native"
    spec_versions: tuple[str, ...] = DEFAULT_SPEC_VERSIONS
    ee_params_path: None = None

    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        entries = {}
        for spec_version in self.spec_versions:
            chunk_hex, acct_hex = _NATIVE_SIGNING_KEYS[spec_version]
            chunk_key_path = datadir / f"native-chunk-signing-key-{spec_version}.hex"
            acct_key_path = datadir / f"native-acct-signing-key-{spec_version}.hex"
            chunk_key_path.write_text(chunk_hex)
            acct_key_path.write_text(acct_hex)
            entries[spec_version] = AlpenProverProgram(
                chunk_path=str(chunk_key_path),
                acct_path=str(acct_key_path),
            )
        return AlpenProverConfig(backend=self.backend, programs=entries)

    @property
    def genesis_predicate(self) -> str:
        return _NATIVE_PREDICATES[self.spec_versions[0]]

    @property
    def rotation_target_predicate(self) -> str:
        self._require_rotation_versions()
        return _NATIVE_PREDICATES[self.spec_versions[1]]


@dataclass(frozen=True)
class Sp1Backend(ProverBackend):
    """The real compiled guest pair from ``provers/sp1``.

    The pair built from current source is always the v1 program -- that is the
    version its guests bake in. A rotation test puts the committed pre-Osaka
    pair underneath it as v0 and launches the chain there, which is the
    production shape: an already-deployed binary at genesis handing off to a
    new one. That is why `genesis_predicate` reads from the test data in that
    case rather than from the build output.
    """

    backend: ClassVar[str] = "sp1"
    spec_versions: tuple[str, ...]
    genesis_predicate: str
    ee_params_path: Path
    programs: dict[str, tuple[Path, Path]]
    """Spec version -> its (chunk ELF, acct ELF) pair."""

    def prover_config(self, datadir: Path) -> AlpenProverConfig:
        return AlpenProverConfig(
            backend=self.backend,
            programs={
                spec_version: AlpenProverProgram(
                    chunk_path=str(chunk_elf),
                    acct_path=str(acct_elf),
                )
                for spec_version, (chunk_elf, acct_elf) in self.programs.items()
            },
        )

    @property
    def rotation_target_predicate(self) -> str:
        self._require_rotation_versions()
        return _require_built(ACCT_PREDICATE).read_text().strip()


#: Shared default for the many call sites that never override the backend.
NATIVE_BACKEND = NativeBackend()


def _require_built(path: Path) -> Path:
    if not path.exists():
        raise RuntimeError(
            f"{path} is missing -- EE_PROVER_BACKEND=sp1 requires run_tests.sh to have built "
            "the SP1 guest pair first (see its build_sp1_guests)"
        )
    return path


def _require_testdata(path: Path) -> Path:
    if not path.exists():
        raise RuntimeError(
            f"{path} is missing -- a spec-version rotation under EE_PROVER_BACKEND=sp1 needs the "
            f"committed pre-Osaka guest pair; see {_SP1_V0_TESTDATA_DIR}/README.md"
        )
    return path


def _sp1_backend(spec_versions: tuple[str, ...]) -> Sp1Backend:
    """Builds the sp1 backend for `spec_versions`.

    The freshly built pair is v1 either way; a rotation additionally puts the
    committed pre-Osaka pair on v0 and launches there -- see [`Sp1Backend`].
    """
    built = (_require_built(CHUNK_ELF), _require_built(ACCT_ELF))
    ee_params = _require_built(EE_PARAMS)

    if spec_versions == DEFAULT_SPEC_VERSIONS:
        return Sp1Backend(
            spec_versions=spec_versions,
            genesis_predicate=_require_built(ACCT_PREDICATE).read_text().strip(),
            ee_params_path=ee_params,
            programs={"v1": built},
        )

    if spec_versions != ROTATION_SPEC_VERSIONS:
        raise ValueError(
            f"the sp1 backend has artifacts for {DEFAULT_SPEC_VERSIONS} and "
            f"{ROTATION_SPEC_VERSIONS} only, not {spec_versions}"
        )
    return Sp1Backend(
        spec_versions=spec_versions,
        genesis_predicate=_require_testdata(V0_ACCT_PREDICATE).read_text().strip(),
        ee_params_path=ee_params,
        programs={
            "v0": (_require_testdata(V0_CHUNK_ELF), _require_testdata(V0_ACCT_ELF)),
            "v1": built,
        },
    )


def resolve_prover_backend(
    spec_versions: tuple[str, ...] = DEFAULT_SPEC_VERSIONS,
) -> ProverBackend:
    """Resolves the prover backend from ``EE_PROVER_BACKEND`` (default: native).

    Args:
        spec_versions: which spec versions need a resident program. Pass
            `ROTATION_SPEC_VERSIONS` for a test that crosses a VK rotation.
    """
    backend = os.environ.get("EE_PROVER_BACKEND", "native")
    if backend == "native":
        return NativeBackend(spec_versions=spec_versions)
    if backend == "sp1":
        return _sp1_backend(spec_versions)
    raise ValueError(f"Unknown EE_PROVER_BACKEND: {backend!r} (expected: native|sp1)")
