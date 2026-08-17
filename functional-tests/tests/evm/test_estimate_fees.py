"""Test the `alpen_estimateFees` RPC (execution gas + Bitcoin-DA fee quote).

The RPC simulates a transaction, measures its gas and state-diff (DA) footprint, and
returns the full fee breakdown including the `effectiveGas` a standard wallet should sign
so its own gas reservation authorizes the separate DA fee.
"""

import logging

import flexitest

from common.accounts import get_dev_account
from common.base_test import AlpenClientTest
from common.config.constants import ServiceType
from envconfigs.alpen_client import AlpenClientEnv

logger = logging.getLogger(__name__)

# Non-zero DA rate (wei per byte) so the DA fee is active for this env.
DA_RATE_WEI_PER_BYTE = 1000

TRANSFER_AMOUNT_WEI = 10**17

# Must mirror the safety margin folded into the quote (crates/reth/rpc fees.rs).
DA_FEE_SAFETY_MARGIN_BPS = 1000
BPS_DENOM = 10000


@flexitest.register
class TestEstimateFees(AlpenClientTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            AlpenClientEnv(
                fullnode_count=0,
                da_rate_wei_per_byte=DA_RATE_WEI_PER_BYTE,
            )
        )

    def main(self, ctx):
        ee_sequencer = self.get_service(ServiceType.AlpenSequencer)
        rpc = ee_sequencer.create_rpc()

        dev_account = get_dev_account(rpc)
        recipient = "0x000000000000000000000000000000000000dEaD"

        request = {
            "from": dev_account.address,
            "to": recipient,
            "value": hex(TRANSFER_AMOUNT_WEI),
        }

        est = rpc.alpen_estimateFees(request)
        logger.info(f"alpen_estimateFees: {est}")

        gas_used = est["gasUsed"]
        base_fee = est["baseFee"]
        diff_size = est["diffSize"]
        da_rate = est["daRate"]
        da_fee = int(est["daFee"], 16)
        effective_gas = est["effectiveGas"]
        total_fee = int(est["totalFee"], 16)

        # The committed DA rate is surfaced verbatim.
        assert da_rate == DA_RATE_WEI_PER_BYTE, f"da_rate {da_rate} != {DA_RATE_WEI_PER_BYTE}"

        # A plain transfer touches accounts, so it has a positive DA footprint.
        assert diff_size > 0, "expected a positive diff_size"
        assert gas_used >= 21000, f"gas_used {gas_used} below the 21000 transfer floor"

        # The quoted DA fee is `da_rate * diff_size` inflated by the safety margin.
        expected_da_fee = da_rate * diff_size * (BPS_DENOM + DA_FEE_SAFETY_MARGIN_BPS) // BPS_DENOM
        assert da_fee == expected_da_fee, f"da_fee {da_fee} != expected {expected_da_fee}"
        assert da_fee > 0, "expected a positive DA fee"

        # effective_gas folds the DA fee into the signed gas limit at the base fee.
        if base_fee > 0:
            expected_da_gas = -(-da_fee // base_fee)  # ceil(da_fee / base_fee)
            assert effective_gas == gas_used + expected_da_gas, (
                f"effective_gas {effective_gas} != gas_used {gas_used} + da_gas {expected_da_gas}"
            )
            assert effective_gas >= gas_used
        else:
            # Without a base-fee floor the DA fee cannot fold into gas.
            assert effective_gas == gas_used

        assert total_fee == gas_used * base_fee + da_fee, (
            f"total_fee {total_fee} != gas_used*base_fee + da_fee"
        )

        # A transaction that sets a gas price above the base fee pays that price (base fee +
        # tip) to the beneficiary, so total_fee must reflect the effective price, not just
        # the base fee.
        gas_price = base_fee + 3_000_000_000  # base fee + 3 gwei tip
        priced_request = {
            "from": dev_account.address,
            "to": recipient,
            "value": hex(TRANSFER_AMOUNT_WEI),
            "gasPrice": hex(gas_price),
        }
        priced = rpc.alpen_estimateFees(priced_request)
        priced_gas_used = priced["gasUsed"]
        priced_da_fee = int(priced["daFee"], 16)
        priced_total = int(priced["totalFee"], 16)
        assert priced_total == priced_gas_used * gas_price + priced_da_fee, (
            f"total_fee {priced_total} != gas_used*gas_price + da_fee "
            f"(gas_price {gas_price} carries a tip above base fee {base_fee})"
        )
        # The tip must make the total strictly larger than a base-fee-only total.
        assert priced_total > priced_gas_used * base_fee + priced_da_fee, (
            "total_fee did not grow when a gas price above the base fee was set"
        )

        # Standard `eth_estimateGas` must return the SAME effective gas, so an unmodified
        # EVM wallet automatically reserves enough gas to cover the DA fee.
        est_gas = int(rpc.eth_estimateGas(request), 16)
        logger.info(
            f"eth_estimateGas (effective) = {est_gas}, alpen effectiveGas = {effective_gas}"
        )
        assert est_gas == effective_gas, (
            f"eth_estimateGas {est_gas} != alpen effectiveGas {effective_gas}"
        )
        # And it must strictly exceed the raw execution gas whenever a DA fee applies.
        if base_fee > 0 and da_fee > 0:
            assert est_gas > gas_used, (
                f"eth_estimateGas {est_gas} did not include DA headroom over raw gas {gas_used}"
            )

        logger.info("alpen_estimateFees and eth_estimateGas return consistent effective gas")
        return True
