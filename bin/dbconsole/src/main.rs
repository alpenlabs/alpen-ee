//! `dbconsole` — an operator console over the Alpen EE store.
//!
//! With no subcommand it is the console: an interactive prompt, a `-c`
//! expression or a `--script` file over the EE MDBX store, decoding values
//! through the node's own codecs. `migrate-sled` fills a fresh MDBX store from
//! the sled store a previous binary wrote. The scripting shell lives in
//! [`dbconsole`]; the codec-owning core lives in `alpen_ee_database::console`.

use std::process;

use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};

mod dbconsole;
mod migrate;

/// A console over the EE MDBX store, and the tools that live beside it.
#[derive(Debug, Parser)]
#[command(
    name = "dbconsole",
    version,
    about,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    console: dbconsole::DbconsoleArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

/// The tools beside the console.
#[derive(Debug, Subcommand)]
enum Command {
    /// Fill a fresh MDBX store from the sled store a previous binary wrote.
    MigrateSled(migrate::MigrateArgs),
}

fn main() {
    // Rust ignores SIGPIPE, so `dbconsole ... | head` would panic on the first
    // write after the reader goes away; a CLI should just stop.
    // SAFETY: `signal` with `SIG_DFL` only restores the default disposition,
    // before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::MigrateSled(args)) => migrate::run(args),
        // clap's `subcommand_negates_reqs` does not reach a flattened
        // argument, so the console's one requirement is checked here.
        None if cli.console.datadir.is_none() => Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "the following required argument was not provided: --datadir <DATADIR>",
            )
            .exit(),
        None => dbconsole::run(cli.console),
    };
    // An operator wants the message, not a report with source locations.
    if let Err(err) = result {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
