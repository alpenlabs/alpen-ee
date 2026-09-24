//! Bitcoin RPC client setup for EE DA reconstruction.

use bitcoin::{Network, Txid};
use bitcoind_async_client::{client::Auth, traits::Reader, Client};
use eyre::{ensure, Context};
use strata_btc_types::BlockHashExt;
use strata_config::BitcoindConfig;
use strata_identifiers::L1BlockCommitment;

/// Creates a Bitcoin RPC client from the supplied configuration.
pub(crate) fn create_bitcoin_rpc_client(config: &BitcoindConfig) -> eyre::Result<Client> {
    // The extraction layer owns retries so one failed request does not pass
    // through two nested retry loops.
    Client::new(
        config.rpc_url.clone(),
        Auth::UserPass(config.rpc_user.clone(), config.rpc_password.clone()),
        Some(0),
        None,
        None,
    )
    .context("failed to create Bitcoin RPC client")
}

/// Ensures the Bitcoin RPC server uses the expected network.
pub(crate) async fn ensure_bitcoin_network(
    client: &Client,
    expected_network: Network,
) -> eyre::Result<()> {
    let actual_network = client
        .network()
        .await
        .context("failed to fetch Bitcoin network")?;
    ensure!(
        actual_network == expected_network,
        "configured Bitcoin network {} does not match RPC network {actual_network}",
        expected_network
    );
    Ok(())
}

/// Looks up the block containing `txid` and returns its hash and height as an L1 commitment.
pub(crate) async fn fetch_transaction_block_commitment(
    client: &Client,
    txid: Txid,
) -> eyre::Result<L1BlockCommitment> {
    let transaction = client
        .get_raw_transaction_verbosity_one(&txid)
        .await
        .wrap_err_with(|| format!("failed to fetch commit transaction {txid}"))?;
    let block_hash = transaction
        .block_hash
        .ok_or_else(|| eyre::eyre!("commit transaction {txid} is not confirmed"))?;
    let height = client
        .get_block_height(&block_hash)
        .await
        .wrap_err_with(|| format!("failed to fetch block height for commit transaction {txid}"))?;
    let height = u32::try_from(height).wrap_err_with(|| {
        format!("block height for commit transaction {txid} exceeds the L1 height range")
    })?;

    Ok(L1BlockCommitment::new(height, block_hash.to_l1_block_id()))
}
