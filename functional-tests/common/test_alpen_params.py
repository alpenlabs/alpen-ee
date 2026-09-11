"""Tests for generated Alpen parameter artifacts."""

import json
import tempfile
import unittest
from pathlib import Path

from common.alpen_params import (
    EEST_BASE_FEE_FLOOR,
    EEST_CHAIN,
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

    def test_eest_chain_predeploys_canonical_system_contracts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            datadir = Path(temporary_directory)
            ee_params_path = datadir / "ee-params.json"
            ee_params_path.write_text(json.dumps({"account_id": "0x01"}))

            params_path = compose_alpen_params(
                datadir,
                ee_params_path,
                chain=EEST_CHAIN,
            )

            params = json.loads(params_path.read_text())

        alloc = params["evm_spec"]["alloc"]
        self.assertEqual(
            alloc["0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe14604d57602036146024575f5ffd5b5f35801560495762001fff810690815414603c575f5ffd5b62001fff01545f5260205ff35b5f5ffd5b62001fff42064281555f359062001fff015500",
        )
        self.assertEqual(
            alloc["0x0000F90827F1C53a10cb7A02335B175320002935"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe14604657602036036042575f35600143038111604257611fff81430311604257611fff9006545f5260205ff35b5f5ffd5b5f35611fff60014303065500",
        )
        self.assertEqual(
            alloc["0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02"]["nonce"],
            "0x1",
        )
        self.assertEqual(
            alloc["0x0000F90827F1C53a10cb7A02335B175320002935"]["nonce"],
            "0x1",
        )
