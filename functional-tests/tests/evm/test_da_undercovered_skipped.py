"""Test that the sequencer skips DA-undercovered transactions.

The DA fee is bounded by a transaction's unused authorized gas. A transaction signed with
no gas headroom (gas_limit == gas_used) leaves nothing to cover its DA fee, so the in-EVM
charge would be capped and the protocol would subsidize its DA cost. The block builder
must therefore refuse to include such a transaction. The sender's nonce is left untouched,
so it can resubmit with the effective gas that `eth_estimateGas` returns.

This is the adversarial counterpart to `test_da_fee.py` (which covers the well-provisioned
path that IS mined and charged).
"""

import logging

import flexitest

from common.accounts import get_dev_account
from common.base_test import AlpenClientTest
from common.config.constants import ServiceType
from common.wait import timeout_for_expected_blocks, wait_until
from envconfigs.alpen_client import AlpenClientEnv

logger = logging.getLogger(__name__)

# Non-zero DA rate so the coverage check is active for this env.
DA_RATE_WEI_PER_BYTE = 1000

TRANSFER_AMOUNT_WEI = 10**17

# Exactly the intrinsic gas of a plain transfer: the tx succeeds but leaves zero unused
# gas budget, so its DA fee cannot be covered.
ZERO_HEADROOM_GAS = 21000

# Number of blocks to let pass while confirming the tx is not mined.
CONFIRM_BLOCKS = 6


@flexitest.register
class TestDaUndercoveredSkipped(AlpenClientTest):
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

        account = get_dev_account(rpc)
        recipient = "0x000000000000000000000000000000000000dEaD"

        nonce_before = int(rpc.eth_getTransactionCount(account.address, "pending"), 16)
        block_before = int(rpc.eth_blockNumber(), 16)

        gas_price = int(rpc.eth_gasPrice(), 16)
        raw_tx = account.sign_transfer(
            to=recipient,
            value=TRANSFER_AMOUNT_WEI,
            gas_price=gas_price,
            gas=ZERO_HEADROOM_GAS,
        )
        tx_hash = rpc.eth_sendRawTransaction(raw_tx)
        logger.info(f"submitted zero-headroom tx {tx_hash} at nonce {nonce_before}")

        # Let several blocks pass. Waiting on block height (not the receipt) proves the
        # chain is live, so a missing receipt means the tx was skipped, not that the chain
        # stalled.
        target_block = block_before + CONFIRM_BLOCKS
        wait_until(
            lambda: int(rpc.eth_blockNumber(), 16) >= target_block,
            error_with="chain did not advance",
            timeout=timeout_for_expected_blocks(CONFIRM_BLOCKS + 2),
        )
        logger.info(f"chain advanced from {block_before} to >= {target_block}")

        # The under-covered tx must not have been mined.
        receipt = rpc.eth_getTransactionReceipt(tx_hash)
        assert receipt is None, f"DA-undercovered tx was mined unexpectedly: {receipt}"

        # And its nonce must not have been consumed (the sender can resubmit).
        nonce_after = int(rpc.eth_getTransactionCount(account.address, "latest"), 16)
        assert nonce_after == nonce_before, (
            f"nonce advanced ({nonce_before} -> {nonce_after}); tx should not have been included"
        )

        logger.info("DA-undercovered transaction was correctly skipped by the sequencer")
        return True
