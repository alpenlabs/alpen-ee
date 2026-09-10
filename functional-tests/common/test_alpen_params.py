"""Tests for generated Alpen parameter artifacts."""

import json
import tempfile
import unittest
from pathlib import Path

from common.alpen_params import (
    EEST_BASE_FEE_FLOOR,
    EEST_GENESIS_BASE_FEE_PER_GAS,
    compose_alpen_params,
)


class AlpenParamsTests(unittest.TestCase):
    """Verify functional-test parameter composition."""

    def test_eest_fee_configuration_changes_only_the_generated_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            datadir = Path(temporary_directory)
            ee_params_path = datadir / "ee-params.json"
            ee_params_path.write_text(json.dumps({"account_id": "0x01"}))

            params_path = compose_alpen_params(
                datadir,
                ee_params_path,
                base_fee_floor=EEST_BASE_FEE_FLOOR,
                genesis_base_fee_per_gas=EEST_GENESIS_BASE_FEE_PER_GAS,
            )

            params = json.loads(params_path.read_text())

        self.assertEqual(params["base_fee_floor"], EEST_BASE_FEE_FLOOR)
        self.assertEqual(
            params["evm_spec"]["baseFeePerGas"],
            hex(EEST_GENESIS_BASE_FEE_PER_GAS),
        )
