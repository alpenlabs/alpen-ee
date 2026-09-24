//! Reconstructs Alpen EVM state from Bitcoin DA and verifies its EE account state against OL.

mod account_state;
mod args;
mod bitcoin;
mod config;
mod da_extraction;
mod evm_state;
mod ol_rpc;
mod output;
mod progress;
mod snapshot;
#[cfg(test)]
mod test_utils;

use clap::Parser;

use crate::{
    account_state::verify_account_state_from_genesis,
    args::{load_alpen_params, Args},
    bitcoin::{
        create_bitcoin_rpc_client, ensure_bitcoin_network, fetch_transaction_block_commitment,
    },
    config::EeDaToolConfig,
    da_extraction::recover_ee_da,
    evm_state::reconstruct_evm_state,
    ol_rpc::RpcOLAccountUpdateSource,
    output::{emit, EeDaVerificationOutcome},
    progress::StageProgress,
    snapshot::save_reconstruction_snapshot,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let params = load_alpen_params(&args.alpen_params)?;
    let config = EeDaToolConfig::from_path(&args.config)?;

    let bitcoin_client = create_bitcoin_rpc_client(config.bitcoind())?;
    ensure_bitcoin_network(&bitcoin_client, config.bitcoind().network).await?;

    let recovered_blobs = recover_ee_da(
        &bitcoin_client,
        &params,
        &config,
        args.start_height,
        args.end_height,
    )
    .await?;

    let reconstruction_progress = StageProgress::new("EVM state reconstruction");
    let reconstruction_outcome = reconstruct_evm_state(&params, recovered_blobs)?;
    reconstruction_progress.finish();

    let Some(reconstruction_outcome) = reconstruction_outcome else {
        let output = EeDaVerificationOutcome::NoEeDaBlobsFound;
        return emit(&output);
    };

    let account_update_source = RpcOLAccountUpdateSource::try_new(config.ol_rpc_url())?;
    let account_state_verification = verify_account_state_from_genesis(
        &params,
        reconstruction_outcome.batch_replay_outcome(),
        &account_update_source,
    )
    .await?;

    let first_batch_l1_ref = reconstruction_outcome.first_batch_l1_ref();
    let last_batch_l1_ref = reconstruction_outcome.last_batch_l1_ref();
    let first_commit_block =
        fetch_transaction_block_commitment(&bitcoin_client, first_batch_l1_ref.commit_txid())
            .await?;
    let last_commit_block = if first_batch_l1_ref.commit_txid() == last_batch_l1_ref.commit_txid() {
        first_commit_block
    } else {
        fetch_transaction_block_commitment(&bitcoin_client, last_batch_l1_ref.commit_txid()).await?
    };
    if let Some(snapshot_path) = args.snapshot.as_deref() {
        save_reconstruction_snapshot(
            snapshot_path,
            reconstruction_outcome.batch_replay_outcome(),
            account_state_verification.verified_state(),
        )?;
    }
    let output = EeDaVerificationOutcome::verified(
        &reconstruction_outcome,
        &account_state_verification,
        first_commit_block,
        last_commit_block,
    );
    emit(&output)
}
