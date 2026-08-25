#!/usr/bin/env python3
"""Generates the AlpenParams fixture ``provers/sp1/build.rs`` bakes into the
guest programs, and prints its path.

Calls the same helpers the harness itself calls, with no overrides, so the
guest-baked params are byte-for-byte what the live node runs with. The same
``ee-params.json`` is threaded into the run by ``common/prover_backend.py``
rather than regenerated on both sides.
"""

from common.alpen_params import compose_alpen_params
from common.datatool import generate_ee_params
from common.prover_backend import ALPEN_PARAMS, GUEST_PARAMS_DIR


def main() -> None:
    GUEST_PARAMS_DIR.mkdir(parents=True, exist_ok=True)
    compose_alpen_params(GUEST_PARAMS_DIR, generate_ee_params(GUEST_PARAMS_DIR))
    print(ALPEN_PARAMS)


if __name__ == "__main__":
    main()
