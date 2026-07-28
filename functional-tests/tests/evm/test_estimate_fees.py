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
                enable_l1_da=True,
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

        logger.info("alpen_estimateFees returned a consistent fee breakdown")
        return True
