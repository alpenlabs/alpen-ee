use alloy_consensus::TxReceipt;
use reth_ethereum_primitives::Receipt;
use revm_primitives::alloy_primitives::Bloom;

/// Accumulates logs bloom from all receipts in the execution output.
///
/// This is a general EVM function that combines blooms from all transaction receipts
/// into a single block-level bloom filter for efficient log filtering.
pub fn accumulate_logs_bloom(receipts: &[Receipt]) -> Bloom {
    let mut logs_bloom = Bloom::default();
    receipts.iter().for_each(|r| {
        logs_bloom.accrue_bloom(&r.bloom());
    });
    logs_bloom
}
