//! `alpen-ee` — operator tooling for the Alpen execution environment.
//!
//! Today it exposes a single subcommand, `dbconsole`: an interactive console
//! over the EE MDBX store that decodes values through the node's own codecs.
//! Future EE operator surfaces (e.g. a Tier-1 `eelog` reader) slot in as
//! sibling subcommands.

use clap::{Parser, Subcommand};

mod dbconsole;

/// Alpen EE operator tooling.
#[derive(Debug, Parser)]
#[command(name = "alpen-ee", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Interactive console over the EE MDBX store (read-only scaffold).
    Dbconsole(dbconsole::DbconsoleArgs),
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Dbconsole(args) => dbconsole::run(args),
    }
}
