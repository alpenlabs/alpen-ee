use argh::FromArgs;
use strata_cli_common::errors::DisplayedError;

use crate::seed::Seed;

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "backup")]
/// Prints a BIP39 mnemonic encoding the internal wallet's seed bytes, in the language the
/// wallet was created (or restored) with
pub struct BackupArgs {}

pub async fn backup(_args: BackupArgs, seed: Seed) -> Result<(), DisplayedError> {
    seed.print_mnemonic();
    Ok(())
}
