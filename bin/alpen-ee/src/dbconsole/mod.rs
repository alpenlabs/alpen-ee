//! The `dbconsole` subcommand.
//!
//! Attaches to the EE store and drives either an interactive REPL or a single
//! `-c` script. The scripting engine (rhai) and value conversion live here; the
//! codec-owning core lives in `alpen_ee_database::console`.

use std::path::PathBuf;

use alpen_ee_database::console::ConsoleDb;
use clap::Args;

mod engine;
mod repl;
mod value;

/// Arguments for `alpen-ee dbconsole`.
#[derive(Debug, Args)]
pub(crate) struct DbconsoleArgs {
    /// EE data directory (the one containing `mdbx/`).
    #[arg(long)]
    datadir: PathBuf,

    /// Allow write verbs. The write path is not implemented in this scaffold,
    /// so this currently only affects the banner.
    #[arg(long, default_value_t = false)]
    allow_writes: bool,

    /// Evaluate a single script string and exit instead of starting the REPL.
    #[arg(short = 'c', long, value_name = "SCRIPT")]
    command: Option<String>,
}

/// Runs the console command.
pub(crate) fn run(args: DbconsoleArgs) -> eyre::Result<()> {
    let db = ConsoleDb::attach_readonly(&args.datadir)?;
    let mut session = repl::Session::new(db);

    println!("attached tier-2 (MDBX) ro · prover env · sequencer: unknown (PID-lock TODO)");
    if args.allow_writes {
        println!(
            "note: --allow-writes was given, but the write path is not implemented in this scaffold"
        );
    }

    match args.command {
        Some(script) => session.eval_and_print(&script),
        None => {
            session.run_repl();
            Ok(())
        }
    }
}
