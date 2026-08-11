"""Sequencer/full-node state parity under the DA fee.

The DA fee is charged in-EVM during block execution. A full node does not trust the
sequencer's result: it re-executes each gossiped block through the engine and would reject
any block whose replayed state root did not match the header. This test confirms the two
nodes converge on the same state once the DA fee is in play — each queried through its own
RPC — so the DA charge is deterministic across the build and re-execution paths.
"""

import logging

import flexitest

from common.accounts import get_dev_account
from common.base_test import AlpenClientTest
from common.config.constants import ServiceType
from common.evm_utils import get_balance, wait_for_receipt
from envconfigs.alpen_client import AlpenClientEnv

logger = logging.getLogger(__name__)

# The DA fee vault predeploy (see alpen-reth-evm constants::DA_FEE_VAULT_ADDRESS).
DA_FEE_VAULT = "0x5400000000000000000000000000000000000003"

# Non-zero DA rate (wei per byte) so the charge is active for this env only.
DA_RATE_WEI_PER_BYTE = 1000

TRANSFER_AMOUNT_WEI = 10**17

# Headroom above the 21000-gas transfer so there is unused gas for the DA fee to draw from.
TX_GAS_LIMIT = 60000


@flexitest.register
class TestDaFeeFullnodeParity(AlpenClientTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            AlpenClientEnv(
                fullnode_count=1,
                da_rate_wei_per_byte=DA_RATE_WEI_PER_BYTE,
            )
        )

    def main(self, ctx):
        ee_sequencer = self.get_service(ServiceType.AlpenSequencer)
        ee_fullnode = self.get_service(ServiceType.AlpenFullNode)

        # The full node must be peered with the sequencer to receive blocks over gossip.
        ee_sequencer.wait_for_peers(1, timeout=30)
        ee_fullnode.wait_for_peers(1, timeout=30)

        seq_rpc = ee_sequencer.create_rpc()
        fn_rpc = ee_fullnode.create_rpc()

        dev_account = get_dev_account(seq_rpc)
        recipient = "0x000000000000000000000000000000000000dEaD"

        # Send a DA-fee-charging transfer to the sequencer (with gas headroom).
        gas_price = int(seq_rpc.eth_gasPrice(), 16)
        raw_tx = dev_account.sign_transfer(
            to=recipient,
            value=TRANSFER_AMOUNT_WEI,
            gas_price=gas_price,
            gas=TX_GAS_LIMIT,
        )
        tx_hash = seq_rpc.eth_sendRawTransaction(raw_tx)
        receipt = wait_for_receipt(seq_rpc, tx_hash)
        assert receipt["status"] == "0x1", f"Transaction failed: {receipt}"

        tx_block = int(receipt["blockNumber"], 16)

        # Wait for the full node to re-execute up to and including the tx's block.
        ee_fullnode.wait_for_block(tx_block)

        # The two nodes must agree on the block at that height. The block hash commits the
        # state root, so an equal hash means the full node replayed the DA charge to the
        # exact same state the sequencer built.
        seq_block = ee_sequencer.get_block_by_number(tx_block)
        fn_block = ee_fullnode.get_block_by_number(tx_block)
        assert seq_block is not None and fn_block is not None, "missing block at tx height"
        assert fn_block["stateRoot"] == seq_block["stateRoot"], (
            f"state root diverged at block {tx_block}: "
            f"sequencer {seq_block['stateRoot']} != full node {fn_block['stateRoot']}"
        )
        assert fn_block["hash"] == seq_block["hash"], (
            f"block hash diverged at block {tx_block}: "
            f"sequencer {seq_block['hash']} != full node {fn_block['hash']}"
        )

        # Each node, from its own RPC, must report the same account balances at that block.
        block_tag = hex(tx_block)
        for addr, label in (
            (DA_FEE_VAULT, "vault"),
            (dev_account.address, "sender"),
            (recipient, "recipient"),
        ):
            seq_bal = get_balance(seq_rpc, addr, block_tag)
            fn_bal = get_balance(fn_rpc, addr, block_tag)
            assert seq_bal == fn_bal, (
                f"{label} balance diverged at block {tx_block}: "
                f"sequencer {seq_bal} != full node {fn_bal}"
            )

        # And the agreed-upon DA fee is non-trivial, so the parity check is not "both zero".
        vault_before = get_balance(fn_rpc, DA_FEE_VAULT, hex(tx_block - 1))
        vault_after = get_balance(fn_rpc, DA_FEE_VAULT, block_tag)
        da_fee = vault_after - vault_before
        assert da_fee > 0, "expected a positive DA fee so the parity check is meaningful"

        logger.info(f"sequencer and full node agree on state at block {tx_block}; DA fee {da_fee} wei")
        return True
