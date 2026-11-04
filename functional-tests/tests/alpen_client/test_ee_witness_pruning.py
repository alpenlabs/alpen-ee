"""The sequencer prunes proof witnesses and accessed-state rows below finality."""

import logging

import flexitest

from common.base_test import BaseTest
from common.config import AlpenPruningConfig, AlpenWitnessPruningConfig
from common.config.constants import ALPEN_ACCOUNT_ID, ServiceType
from common.dbconsole import eval_store_ints
from common.services.alpen_client import AlpenClientService
from common.services.bitcoin import BitcoinService
from common.services.strata import StrataService
from common.wait import wait_until_with_value
from envconfigs.el_ol import EeOLEnv

logger = logging.getLogger(__name__)

# A tight margin and a fast pass, so one finality step prunes rows; production
# keeps 1000 blocks and passes once a minute.
KEEP_BLOCKS = 5
PRUNE_INTERVAL_SECS = 2
# Finalizing this block (finality lands on an epoch's last block, so usually a
# later one) makes the pruner drop every height below finalized - KEEP_BLOCKS.
TARGET_BLOCK = 12

# The store as the pruner leaves it: the finalized height, the tip, the row
# counts of the two pruned tables, the block-record count, and the lowest
# finalized height whose block still has a witness (-1 if none). Each verb is
# its own read transaction, so a sample taken while a block lands can be
# off by one row; the test waits for a sample where everything agrees.
STORE_PROBE = """
let tip = parse_int(last("ExecBlocksAtHeightSchema").key);
let fin = parse_int(last("ExecBlockFinalizedSchema").key);
let first = -1;
let h = 1;
while h <= fin && first < 0 {
    let row = get("ExecBlockFinalizedSchema", h.to_string());
    if row != () && get("BlockWitnessSchema", row.value) != () { first = h; }
    h += 1;
}
#{
    tip: tip,
    finalized: fin,
    witnesses: count("BlockWitnessSchema"),
    accessed: count("BlockAccessedStateSchema"),
    blocks: count("ExecBlockSchema"),
    first_with_witness: first,
}
"""


def pruned_to_margin(probe: dict[str, int]) -> bool:
    """Rows below finalized - KEEP_BLOCKS are gone, everything above is intact."""
    first = probe["first_with_witness"]
    return (
        first == probe["finalized"] - KEEP_BLOCKS
        # Every height from the margin to the tip keeps its rows (genesis has none).
        and probe["witnesses"] == probe["tip"] - first + 1
        and probe["accessed"] == probe["witnesses"]
        # Block records are untouched: one per height, genesis included.
        and probe["blocks"] == probe["tip"] + 1
    )


@flexitest.register
class TestEeWitnessPruning(BaseTest):
    def __init__(self, ctx: flexitest.InitContext):
        ctx.set_env(
            EeOLEnv(
                pre_generate_blocks=110,
                pruning=AlpenPruningConfig(
                    witness=AlpenWitnessPruningConfig(
                        keep_blocks=KEEP_BLOCKS, interval_secs=PRUNE_INTERVAL_SECS
                    )
                ),
            )
        )

    def main(self, ctx):
        alpen_seq: AlpenClientService = self.get_service(ServiceType.AlpenSequencer)
        strata_seq: StrataService = self.get_service(ServiceType.Strata)
        bitcoin: BitcoinService = self.get_service(ServiceType.Bitcoin)
        datadir = alpen_seq.props["datadir"]

        strata_rpc = strata_seq.wait_for_rpc_ready(timeout=10)
        strata_seq.wait_for_account_genesis_epoch_commitment(
            ALPEN_ACCOUNT_ID, rpc=strata_rpc, timeout=20
        )
        alpen_seq.wait_for_block(TARGET_BLOCK + 1, timeout=60)
        target_hash = alpen_seq.get_block_by_number(TARGET_BLOCK)["hash"]

        # Before finality nothing is pruned: block 1 still has its witness.
        before = eval_store_ints(datadir, STORE_PROBE)
        logger.info("store before finality: %s", before)
        assert before["first_with_witness"] in (-1, 1), before

        # Mine L1 until the target block is finalized.
        bitcoin.mine_until(
            check=lambda: alpen_seq.get_block_status(target_hash)["status"],
            predicate=lambda s: s == "finalized",
            error_with=f"Block {TARGET_BLOCK} did not finalize",
        )

        # The pruner's next pass drops everything below finalized - KEEP_BLOCKS;
        # finality may still move once more after the last mined L1 block.
        after = wait_until_with_value(
            lambda: eval_store_ints(datadir, STORE_PROBE),
            pruned_to_margin,
            error_with="the store never showed the rows below finalized - keep_blocks pruned",
            timeout=30,
        )
        logger.info("store after pruning: %s", after)
        assert alpen_seq.get_block_status(target_hash)["status"] == "finalized"

        # The chain keeps going with the pruner running.
        alpen_seq.wait_for_block(after["tip"] + 3, timeout=30)
        logger.info("witness pruning test passed")
