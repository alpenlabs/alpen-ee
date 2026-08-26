use std::collections::BTreeMap;

use argh::FromArgs;
use bdk_wallet::{
    bitcoin::{secp256k1::SECP256K1, Amount, FeeRate, PrivateKey},
    chain::ChainOracle,
    coin_selection::InsufficientFunds,
    descriptor::IntoWalletDescriptor,
    error::CreateTxError,
    KeychainKind, Wallet,
};
use chrono::Utc;
use colored::Colorize;
use strata_cli_common::errors::{DisplayableError, DisplayedError};
use strata_primitives::crypto::even_kp;

use crate::{
    cmd::deposit::bridge_in_descriptor,
    constants::{RECOVERY_DESC_CLEANUP_DELAY, SEED_RECOVERY_GAP_LIMIT},
    link::{OnchainObject, PrettyPrint},
    recovery::DescriptorRecovery,
    seed::Seed,
    settings::Settings,
    signet::{get_fee_rate, log_fee_rate, sync_wallet, SignetWallet},
};

/// Attempts a recovery of old deposit transactions
#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "recover")]
pub struct RecoverArgs {
    /// override signet fee rate in sat/vbyte. must be >=1
    #[argh(option)]
    fee_rate: Option<u64>,
}

/// Returns whether an already-claimed descriptor's cleanup grace window has elapsed.
fn cleanup_delay_elapsed(recover_at: u32, current_height: u32) -> bool {
    current_height >= recover_at.saturating_add(RECOVERY_DESC_CLEANUP_DELAY)
}

pub async fn recover(
    args: RecoverArgs,
    seed: Seed,
    settings: Settings,
) -> Result<(), DisplayedError> {
    let mut l1w = SignetWallet::new(&seed, settings.network, settings.signet_backend.clone())
        .internal_error("Failed to load signet wallet")?;
    l1w.sync()
        .await
        .internal_error("Failed to sync signet wallet")?;

    println!("Opening descriptor recovery");
    let mut descriptor_file = DescriptorRecovery::open(&seed, &settings.descriptor_db)
        .await
        .internal_error("Failed to open descriptor recovery file")?;
    let current_height = l1w
        .local_chain()
        .get_chain_tip()
        .expect("valid chain tip")
        .height;

    println!("Current signet chain height: {current_height}");
    let descs = descriptor_file
        .read_descs(..=current_height)
        .await
        .internal_error("Failed to read descriptors after chain height")?;

    if descs.is_empty() {
        println!("No descriptors in the local database");
    }

    let fee_rate = get_fee_rate(args.fee_rate, settings.signet_backend.as_ref()).await;
    log_fee_rate(&fee_rate);

    for (key, desc) in descs {
        let desc = desc
            .clone()
            .into_wallet_descriptor(l1w.secp_ctx(), settings.network)
            .internal_error("Failed to convert to wallet descriptor")?;

        let mut recovery_wallet = Wallet::create_single(desc)
            .network(settings.network)
            .create_wallet_no_persist()
            .internal_error("Failed to create recovery wallet")?;

        // reveal the address for the wallet so we can sync it
        let address = recovery_wallet.reveal_next_address(KeychainKind::External);
        sync_wallet(&mut recovery_wallet, settings.signet_backend.clone())
            .await
            .internal_error("Failed to sync recovery wallet")?;
        let needs_recovery = recovery_wallet.balance().confirmed > Amount::ZERO;

        if !needs_recovery {
            if cleanup_delay_elapsed(key.recover_at, current_height) {
                descriptor_file
                    .remove(&key)
                    .internal_error("Failed to remove old descriptor")?;
                println!(
                    "removed old, already claimed descriptor due for recovery at {}",
                    key.recover_at
                );
            }
            continue;
        }

        println!(
            "Recovering a deposit transaction from recovery address {}",
            address.to_string().yellow(),
        );
        drain_recovery_path(&mut recovery_wallet, &mut l1w, &settings, fee_rate).await?;
    }

    recover_from_seed(&seed, &settings, &mut l1w, fee_rate).await?;

    Ok(())
}

/// Drains `recovery_wallet`'s reclaim path (policy path index 1: recovery pubkey + timelock,
/// see [`bridge_in_descriptor`]) to `l1w`, signing and broadcasting the spend.
async fn drain_recovery_path(
    recovery_wallet: &mut Wallet,
    l1w: &mut Wallet,
    settings: &Settings,
    fee_rate: FeeRate,
) -> Result<(), DisplayedError> {
    recovery_wallet.transactions().for_each(|tx| {
        l1w.apply_unconfirmed_txs([(tx.tx_node.tx, Utc::now().timestamp() as u64)]);
    });

    let recover_to = l1w.reveal_next_address(KeychainKind::External).address;
    println!(
        "Recovering to wallet address {}",
        recover_to.to_string().yellow()
    );

    let policy = recovery_wallet
        .policies(KeychainKind::External)
        .expect("valid descriptor use")
        .expect("a policy");

    // we want to drain the recovery path to the l1 wallet
    let mut psbt = {
        let mut builder = recovery_wallet.build_tx();
        // we want to spend via the 2nd option - the recovery + delay
        builder.policy_path(
            BTreeMap::from([(policy.id, vec![1])]),
            KeychainKind::External,
        );
        builder.drain_wallet();
        builder.drain_to(recover_to.script_pubkey());
        builder.fee_rate(fee_rate);
        match builder.finish() {
            Ok(psbt) => psbt,
            Err(CreateTxError::CoinSelection(e @ InsufficientFunds { .. })) => {
                return Err(DisplayedError::UserError(
                    "Failed to create PSBT".to_string(),
                    Box::new(e),
                ));
            }
            Err(e) => panic!("Unexpected error in creating PSBT: {e:?}"),
        }
    };

    assert!(
        recovery_wallet
            .sign(&mut psbt, Default::default())
            .expect("sign to be ok"),
        "transaction should be finalized"
    );

    let tx = psbt.extract_tx().expect("tx should be signed and ready");
    settings
        .signet_backend
        .broadcast_tx(&tx)
        .await
        .internal_error("Failed to broadcast signet transaction")?;

    println!(
        "{}",
        OnchainObject::from(&tx.compute_txid())
            .with_maybe_explorer(settings.mempool_space_endpoint.as_deref())
            .pretty()
    );

    Ok(())
}

/// Reconstructs and recovers deposits directly from the seed, for deposits whose descriptor DB
/// entry is missing. Tries `seed.drt_reclaim_keypair(counter)` for `counter = 0, 1, 2, ...`,
/// rebuilding each candidate's descriptor with the network's *current* bridge pubkey and
/// recovery delay -- if either has changed since a given deposit was created, that deposit won't
/// be found here. Stops after [`SEED_RECOVERY_GAP_LIMIT`] consecutive counters with no on-chain
/// history, the same convention BIP44 uses for address-gap discovery.
async fn recover_from_seed(
    seed: &Seed,
    settings: &Settings,
    l1w: &mut Wallet,
    fee_rate: FeeRate,
) -> Result<(), DisplayedError> {
    println!("Scanning for deposits reconstructable from the seed alone...");

    let mut found_any = false;
    let mut consecutive_misses = 0;
    let mut counter = 0u32;
    while consecutive_misses < SEED_RECOVERY_GAP_LIMIT {
        let (secret_key, _) = even_kp(seed.drt_reclaim_keypair(counter));
        let recovery_private_key = PrivateKey::new(secret_key.into(), settings.network);
        let desc = bridge_in_descriptor(
            settings.bridge_musig2_pubkey,
            recovery_private_key,
            settings.recovery_delay,
        );

        let wallet_desc = desc
            .into_wallet_descriptor(SECP256K1, settings.network)
            .internal_error("Failed to convert to wallet descriptor")?;
        let mut recovery_wallet = Wallet::create_single(wallet_desc)
            .network(settings.network)
            .create_wallet_no_persist()
            .internal_error("Failed to create recovery wallet")?;

        let address = recovery_wallet.reveal_next_address(KeychainKind::External);
        sync_wallet(&mut recovery_wallet, settings.signet_backend.clone())
            .await
            .internal_error("Failed to sync recovery wallet")?;

        if recovery_wallet.transactions().next().is_none() {
            consecutive_misses += 1;
            counter += 1;
            continue;
        }
        consecutive_misses = 0;

        if recovery_wallet.balance().confirmed > Amount::ZERO {
            found_any = true;
            println!(
                "Recovering a deposit transaction (counter {counter}) from recovery address {}",
                address.to_string().yellow(),
            );
            drain_recovery_path(&mut recovery_wallet, l1w, settings, fee_rate).await?;
        }

        counter += 1;
    }

    if !found_any {
        println!(
            "Nothing found to recover from the seed within {SEED_RECOVERY_GAP_LIMIT} unused \
             counters past the last one with on-chain activity."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_delay_not_elapsed_keeps_descriptor() {
        let recover_at = 1_000;

        assert!(!cleanup_delay_elapsed(recover_at, recover_at));
        assert!(!cleanup_delay_elapsed(
            recover_at,
            recover_at + RECOVERY_DESC_CLEANUP_DELAY - 1
        ));
    }

    #[test]
    fn test_cleanup_delay_exactly_elapsed_removes_descriptor() {
        let recover_at = 1_000;

        assert!(cleanup_delay_elapsed(
            recover_at,
            recover_at + RECOVERY_DESC_CLEANUP_DELAY
        ));
    }

    #[test]
    fn test_cleanup_delay_well_past_removes_descriptor() {
        let recover_at = 1_000;

        assert!(cleanup_delay_elapsed(
            recover_at,
            recover_at + RECOVERY_DESC_CLEANUP_DELAY + 1_000
        ));
    }

    #[test]
    fn test_cleanup_delay_saturates_near_max_height() {
        let recover_at = u32::MAX;

        assert!(cleanup_delay_elapsed(recover_at, u32::MAX));
        assert!(!cleanup_delay_elapsed(recover_at, u32::MAX - 1));
    }
}
