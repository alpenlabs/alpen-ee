//! Reconstructs Alpen EVM state from EE DA blobs published on Bitcoin.

mod args;
mod bitcoin;
mod config;
mod da_extraction;
mod evm_state;
mod output;
mod progress;

use clap::Parser;

use crate::{
    args::{load_alpen_params, Args},
    bitcoin::{
        create_bitcoin_rpc_client, ensure_bitcoin_network, fetch_transaction_block_commitment,
    },
    config::EeDaToolConfig,
    da_extraction::recover_ee_da,
    evm_state::reconstruct_evm_state,
    output::{emit, EvmStateReconstructionOutcome},
    progress::StageProgress,
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
        let output = EvmStateReconstructionOutcome::NoEeDaBlobsFound;
        return emit(&output);
    };

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
    let output = EvmStateReconstructionOutcome::complete(
        &reconstruction_outcome,
        first_commit_block,
        last_commit_block,
    );
    emit(&output)
}
