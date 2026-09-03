use bitcoind_async_client::corepc_types::bitcoin::FeeRate;
use strata_config::btcio::{FeePolicy, L1FeePolicyConfig};

use super::{
    start,
    test_support::{configured_rate, disconnected_bitcoin_client, TestExecutor},
};
use crate::config::DaFeeRatePolicyConfig;

#[tokio::test]
async fn fixed_config_starts_with_adjusted_policy_rate() {
    let config = configured_rate(DaFeeRatePolicyConfig::Fixed {
        rate_wei_per_byte: 7,
    });
    let executor = TestExecutor::default();
    let service_handle = start(
        &config,
        disconnected_bitcoin_client(),
        L1FeePolicyConfig::new(FeePolicy::Fixed {
            fee_rate: FeeRate::from_sat_per_kwu(1),
        }),
        &executor,
    )
    .await
    .unwrap();

    assert_eq!(service_handle.rate_handle().current_rate(), 14);
    assert!(service_handle.stop());
    executor.join().await.unwrap();
}

#[tokio::test]
async fn writer_backed_config_reuses_writer_fee_policy() {
    let config = configured_rate(DaFeeRatePolicyConfig::WriterBacked);
    let executor = TestExecutor::default();
    let service_handle = start(
        &config,
        disconnected_bitcoin_client(),
        L1FeePolicyConfig::new(FeePolicy::Fixed {
            fee_rate: FeeRate::from_sat_per_kwu(125),
        }),
        &executor,
    )
    .await
    .unwrap();

    assert_eq!(service_handle.rate_handle().current_rate(), 1_875_000_003);
    assert!(service_handle.stop());
    executor.join().await.unwrap();
}
