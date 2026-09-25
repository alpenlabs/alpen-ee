"""Verify writer-backed DA-rate adjustments take effect after restart."""

import logging
from pathlib import Path
from typing import cast

import flexitest
import toml

from common.accounts import get_dev_account
from common.base_test import BaseTest
from common.config import AlpenDaFeeRateConfig, AlpenL1FeePolicyConfig
from common.config.constants import DEV_RECIPIENT_ADDRESS, SATS_TO_WEI, ServiceType
from common.evm_utils import get_balance, wait_for_receipt
from common.services import AlpenClientService
from common.wait import wait_until_with_value
from envconfigs.alpen_client import AlpenClientEnv

logger = logging.getLogger(__name__)

L1_FEE_RATE_SAT_VB = 1
WEIGHT_UNITS_PER_VBYTE = 4
POLICY_RATE_WEI_PER_BYTE = L1_FEE_RATE_SAT_VB * SATS_TO_WEI // WEIGHT_UNITS_PER_VBYTE
UPDATED_MULTIPLIER_BPS = 5_000
UPDATED_OFFSET_WEI_PER_BYTE = 17
UPDATED_RATE_WEI_PER_BYTE = (
    POLICY_RATE_WEI_PER_BYTE * UPDATED_MULTIPLIER_BPS // 10_000 + UPDATED_OFFSET_WEI_PER_BYTE
)
TRANSFER_AMOUNT_WEI = 10**17


def _da_rate_from_block(block: dict) -> int:
    """Decode the big-endian DA-rate body from Alpen header extra data."""
    extra_data_hex = block["extraData"].removeprefix("0x")
    extra_data = bytes.fromhex(extra_data_hex)
    assert len(extra_data) == 10, f"unexpected Alpen extraData length: {len(extra_data)}"
    return int.from_bytes(extra_data[2:], "big")


@flexitest.register
class TestDaFeeRateRestartTest(BaseTest):
    """Verify a restarted sequencer applies updated writer-backed adjustment settings."""

    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            AlpenClientEnv(
                fullnode_count=0,
                l1_fee_policy=AlpenL1FeePolicyConfig(
                    fee_policy="fixed",
                    fixed_fee_rate=L1_FEE_RATE_SAT_VB,
                ),
                da_fee_rate=AlpenDaFeeRateConfig(
                    policy="writer_backed",
                    fixed_rate_wei_per_byte=None,
                    refresh_interval_seconds=1,
                    stale_after_seconds=30,
                    explorer_timeout_seconds=5,
                    bitcoind_timeout_seconds=5,
                    multiplier_bps=10_000,
                    offset_wei_per_byte=0,
                    min_rate_wei_per_byte=1,
                    max_rate_wei_per_byte=POLICY_RATE_WEI_PER_BYTE,
                ),
            )
        )

    def main(self, ctx) -> bool:
        sequencer = cast(
            AlpenClientService,
            self.runctx.get_service(ServiceType.AlpenSequencer),
        )

        # Phase 1: the initial writer-backed rate reaches both block headers and fee estimates.
        initial_height, initial_rate = self._wait_for_rate(sequencer, POLICY_RATE_WEI_PER_BYTE)
        rpc = sequencer.create_rpc()
        account = get_dev_account(rpc)
        request = {
            "from": account.address,
            "to": DEV_RECIPIENT_ADDRESS,
            "value": hex(TRANSFER_AMOUNT_WEI),
        }
        initial_estimate = self._assert_estimate(rpc, request, POLICY_RATE_WEI_PER_BYTE)
        logger.info(
            "Observed initial writer-backed DA rate %s at block %s",
            initial_rate,
            initial_height,
        )

        # Phase 2: changing the affine adjustment across a restart changes only the DA quote.
        sequencer.stop()
        self._update_adjustment_config(sequencer)
        sequencer.start()
        sequencer.wait_for_ready(timeout=60)
        rpc = sequencer.create_rpc()

        updated_height, updated_rate = self._wait_for_rate(
            sequencer,
            UPDATED_RATE_WEI_PER_BYTE,
            after_height=initial_height,
        )
        logger.info(
            "Observed adjusted writer-backed DA rate %s at block %s after restart",
            updated_rate,
            updated_height,
        )

        updated_estimate = self._assert_estimate(rpc, request, UPDATED_RATE_WEI_PER_BYTE)
        assert updated_estimate["gasUsed"] == initial_estimate["gasUsed"]
        assert updated_estimate["diffSize"] == initial_estimate["diffSize"]
        assert int(updated_estimate["daFee"], 16) < int(initial_estimate["daFee"], 16)

        # Phase 3: a real transaction pays the adjusted DA fee. Receipts expose execution gas,
        # so reconcile that with the sender's balance delta to isolate the separate DA charge.
        sender_before = get_balance(rpc, account.address)
        gas_price = int(rpc.eth_gasPrice(), 16)
        raw_tx = account.sign_transfer(
            to=DEV_RECIPIENT_ADDRESS,
            value=TRANSFER_AMOUNT_WEI,
            gas_price=gas_price,
            gas=updated_estimate["effectiveGas"],
        )
        tx_hash = rpc.eth_sendRawTransaction(raw_tx)
        receipt = wait_for_receipt(rpc, tx_hash)
        assert receipt["status"] == "0x1", f"transaction failed: {receipt}"
        receipt_block = sequencer.get_block_by_number(receipt["blockNumber"])
        assert receipt_block is not None
        assert _da_rate_from_block(receipt_block) == UPDATED_RATE_WEI_PER_BYTE

        gas_used = int(receipt["gasUsed"], 16)
        effective_gas_price = int(receipt["effectiveGasPrice"], 16)
        sender_debit = sender_before - get_balance(rpc, account.address)
        charged_da_fee = sender_debit - TRANSFER_AMOUNT_WEI - gas_used * effective_gas_price
        expected_da_fee = UPDATED_RATE_WEI_PER_BYTE * updated_estimate["diffSize"]
        assert charged_da_fee == expected_da_fee, (
            f"charged DA fee {charged_da_fee} != expected {expected_da_fee}"
        )
        assert gas_used == updated_estimate["gasUsed"], (
            "receipt gasUsed should remain the raw execution gas"
        )
        return True

    @staticmethod
    def _assert_estimate(rpc, request: dict, expected_rate: int) -> dict:
        estimate = rpc.alpen_estimateFees(request)
        assert estimate["daRate"] == expected_rate
        estimated_gas = int(rpc.eth_estimateGas(request), 16)
        assert estimated_gas == estimate["effectiveGas"]
        return estimate

    @staticmethod
    def _wait_for_rate(
        sequencer: AlpenClientService,
        expected_rate: int,
        after_height: int = 0,
    ) -> tuple[int, int]:
        def latest_height_and_rate() -> tuple[int, int]:
            height = sequencer.get_block_number()
            block = sequencer.get_block_by_number(height)
            assert block is not None, f"block {height} was not found"
            return height, _da_rate_from_block(block)

        return wait_until_with_value(
            latest_height_and_rate,
            lambda observed: observed[0] > after_height and observed[1] == expected_rate,
            error_with=f"DA rate did not become {expected_rate} wei per byte",
            timeout=60,
            step=1,
        )

    @staticmethod
    def _update_adjustment_config(sequencer: AlpenClientService) -> None:
        config_path = Path(sequencer.props["datadir"]) / "alpen-config.toml"
        config = toml.load(config_path)
        da_fee_rate = config["sequencer"]["da_fee_rate"]
        da_fee_rate["multiplier_bps"] = UPDATED_MULTIPLIER_BPS
        da_fee_rate["offset_wei_per_byte"] = UPDATED_OFFSET_WEI_PER_BYTE
        config_path.write_text(toml.dumps(config))
