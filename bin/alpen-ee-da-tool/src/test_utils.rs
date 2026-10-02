use alpen_ee_batch_replay::{replay_from_genesis, BatchReplayOutcome, EvmReplayBatch};
use alpen_ee_da_types::EvmHeaderSummary;
use alpen_reth_statediff::BatchStateDiff;
use strata_snark_acct_types::Seqno;

pub(crate) fn replay_empty_batch() -> BatchReplayOutcome {
    replay_from_genesis(
        [],
        [EvmReplayBatch::new(
            Seqno::zero(),
            EvmHeaderSummary {
                block_num: 1,
                timestamp: 1,
                base_fee: 1,
                gas_used: 0,
                gas_limit: 1,
            },
            BatchStateDiff::new(),
        )],
    )
    .expect("batch replays")
}
