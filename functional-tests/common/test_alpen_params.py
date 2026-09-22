"""Tests for generated Alpen parameter artifacts."""

import json
import tempfile
import unittest
from pathlib import Path

from common.alpen_params import (
    DEFAULT_BASE_FEE_FLOOR,
    EEST_BASE_FEE_FLOOR,
    EEST_BLOCK_GAS_LIMIT,
    EEST_CHAIN,
    EEST_GENESIS_BASE_FEE_PER_GAS,
    EEST_MAX_TX_INPUT_BYTES,
    EEST_RPC_TX_FEE_CAP,
    compose_alpen_params,
    resolve_base_fee_settings,
)
from entry import make_eest_proof_env
from envconfigs.alpen_client import AlpenClientEnv, AlpenClientEnvParams
from envconfigs.el_ol import EeOLEnv


class AlpenParamsTests(unittest.TestCase):
    """Verify functional-test parameter composition."""

    def test_eest_transaction_admission_limits_cover_reviewed_vectors(self) -> None:
        self.assertEqual(EEST_BLOCK_GAS_LIMIT, 120_000_000)
        self.assertGreaterEqual(EEST_MAX_TX_INPUT_BYTES, 1_231_210)
        self.assertEqual(EEST_RPC_TX_FEE_CAP, 0)

    def test_scheduled_eest_keeps_the_proving_environment(self) -> None:
        env = make_eest_proof_env()
        self.assertFalse(env.alpen_env_params.eest_fixture_mode)
        self.assertEqual(env.alpen_env_params.base_fee_floor, EEST_BASE_FEE_FLOOR)
        self.assertEqual(
            env.alpen_env_params.genesis_base_fee_per_gas,
            EEST_GENESIS_BASE_FEE_PER_GAS,
        )

    def test_fixture_envs_use_canonical_eest_fees(self) -> None:
        for params in (
            AlpenClientEnv(fullnode_count=0, eest_fixture_mode=True).env_params,
            EeOLEnv(fullnode_count=0, eest_fixture_mode=True).alpen_env_params,
            AlpenClientEnvParams(
                fullnode_count=0,
                enable_discovery=False,
                pure_discovery=False,
                mesh_bootnodes=False,
                eest_fixture_mode=True,
            ),
        ):
            self.assertEqual(params.base_fee_floor, EEST_BASE_FEE_FLOOR)
            self.assertEqual(params.genesis_base_fee_per_gas, EEST_GENESIS_BASE_FEE_PER_GAS)

    def test_fixture_rejects_incompatible_explicit_fees(self) -> None:
        with self.assertRaisesRegex(ValueError, "zero base fee floor"):
            resolve_base_fee_settings(True, DEFAULT_BASE_FEE_FLOOR, None)
        with self.assertRaisesRegex(ValueError, "7 wei genesis base fee"):
            resolve_base_fee_settings(True, None, 8)

    def test_fixture_requires_isolated_sequencer(self) -> None:
        with self.assertRaisesRegex(ValueError, "fullnode_count=0"):
            AlpenClientEnv(eest_fixture_mode=True)

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
        self.assertEqual(int(params["evm_spec"]["gasLimit"], 16), EEST_BLOCK_GAS_LIMIT)
        self.assertEqual(
            alloc["0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe14604d57602036146024575f5ffd5b5f35801560495762001fff810690815414603c575f5ffd5b62001fff01545f5260205ff35b5f5ffd5b62001fff42064281555f359062001fff015500",
        )
        self.assertEqual(
            alloc["0x0000F90827F1C53a10cb7A02335B175320002935"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe14604657602036036042575f35600143038111604257611fff81430311604257611fff9006545f5260205ff35b5f5ffd5b5f35611fff60014303065500",
        )
        self.assertEqual(
            alloc["0x00000961Ef480Eb55e80D19ad83579A64c007002"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe1460cb5760115f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff146101f457600182026001905f5b5f82111560685781019083028483029004916001019190604d565b909390049250505036603814608857366101f457346101f4575f5260205ff35b34106101f457600154600101600155600354806003026004013381556001015f35815560010160203590553360601b5f5260385f601437604c5fa0600101600355005b6003546002548082038060101160df575060105b5f5b8181146101835782810160030260040181604c02815460601b8152601401816001015481526020019060020154807fffffffffffffffffffffffffffffffff00000000000000000000000000000000168252906010019060401c908160381c81600701538160301c81600601538160281c81600501538160201c81600401538160181c81600301538160101c81600201538160081c81600101535360010160e1565b910180921461019557906002556101a0565b90505f6002555f6003555b5f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff14156101cd57505f5b6001546002828201116101e25750505f6101e8565b01600290035b5f555f600155604c025ff35b5f5ffd",
        )
        self.assertEqual(
            alloc["0x0000BBdDc7CE488642fb579F8B00f3a590007251"]["code"],
            "0x3373fffffffffffffffffffffffffffffffffffffffe1460d35760115f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff1461019a57600182026001905f5b5f82111560685781019083028483029004916001019190604d565b9093900492505050366060146088573661019a573461019a575f5260205ff35b341061019a57600154600101600155600354806004026004013381556001015f358155600101602035815560010160403590553360601b5f5260605f60143760745fa0600101600355005b6003546002548082038060021160e7575060025b5f5b8181146101295782810160040260040181607402815460601b815260140181600101548152602001816002015481526020019060030154905260010160e9565b910180921461013b5790600255610146565b90505f6002555f6003555b5f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff141561017357505f5b6001546001828201116101885750505f61018e565b01600190035b5f555f6001556074025ff35b5f5ffd",
        )
        self.assertEqual(
            alloc["0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02"]["nonce"],
            "0x1",
        )
        self.assertEqual(
            alloc["0x0000F90827F1C53a10cb7A02335B175320002935"]["nonce"],
            "0x1",
        )
        for address in (
            "0x00000961Ef480Eb55e80D19ad83579A64c007002",
            "0x0000BBdDc7CE488642fb579F8B00f3a590007251",
        ):
            self.assertEqual(alloc[address]["nonce"], "0x1")
            self.assertEqual(
                alloc[address]["storage"]["0x" + "00" * 32],
                "0x" + "ff" * 32,
            )
