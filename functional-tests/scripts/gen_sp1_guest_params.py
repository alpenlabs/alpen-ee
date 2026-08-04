#!/usr/bin/env python3
"""Generates the deterministic AlpenParams fixtures baked into the v0/v1 SP1
guest programs built by ``provers/sp1-func-test-guests/build.rs``, used by
``EE_PROVER_BACKEND=sp1``'s variant of ``test_ee_predicate_transition.py``.

Calls the exact same datatool/composition helpers the functional-test harness
itself calls, with no overrides, so the guest-baked v0 params are
byte-for-byte identical to what the live test run's EE node uses (see
``common/prover_backend.py``, which threads this same ``ee-params.json`` into
the live run via ``StrataFactory.create_node``'s ``ee_params_path_override``,
rather than relying on both sides independently regenerating the same
content). v1 differs only in ``blob_spec.magic_bytes`` -- confirmed unread by
both ``crates/proof-impl/alpen-{chunk,acct}``, so it cannot affect real STF
verification, making it the only field that's safe to vary purely to produce
a second, distinct compiled guest pair (and hence a second, distinct real
predicate) for the rotation test to rotate into.
"""

import argparse
from pathlib import Path

from common.alpen_params import compose_alpen_params
from common.datatool import generate_ee_params

V1_DA_MAGIC_BYTES = "ALP1"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out-dir", required=True, type=Path, help="Directory to write the generated fixtures to"
    )
    args = parser.parse_args()

    out_dir: Path = args.out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    ee_params_path = generate_ee_params(out_dir)

    v0_dir = out_dir / "v0"
    v0_dir.mkdir(exist_ok=True)
    v0_params_path = compose_alpen_params(v0_dir, ee_params_path)

    v1_dir = out_dir / "v1"
    v1_dir.mkdir(exist_ok=True)
    v1_params_path = compose_alpen_params(
        v1_dir, ee_params_path, da_magic_bytes=V1_DA_MAGIC_BYTES
    )

    # Consumed by run_tests.sh to export env vars for the Python backend
    # resolver (common/prover_backend.py).
    print(f"ee_params_path={ee_params_path}")
    print(f"v0_alpen_params_path={v0_params_path}")
    print(f"v1_alpen_params_path={v1_params_path}")


if __name__ == "__main__":
    main()
