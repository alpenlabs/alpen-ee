//! The console: its arguments and its run loop.
//!
//! Attaches to the EE store and drives an interactive prompt, a single `-c`
//! expression, or a `--script` file. The scripting engine (rhai), the session
//! that persists functions between inputs, and value conversion live here;
//! the codec-owning core lives in `alpen_ee_database::console`.

use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use alpen_ee_database::console::{AttachMode, ConsoleDb};
use clap::Args;
use signal_hook::{consts::SIGINT, flag};

mod engine;
mod handle;
pub(crate) mod recipes;
pub(crate) mod repl;
pub(crate) mod session;
mod value;

/// Arguments for the console itself.
#[derive(Debug, Args)]
pub(crate) struct DbconsoleArgs {
    /// EE data directory (the one containing `mdbx/`).
    ///
    /// Required for the console; `main` enforces it, because clap does not
    /// lift a flattened requirement when a subcommand is given.
    #[arg(long)]
    pub(crate) datadir: Option<PathBuf>,

    /// Allow write verbs.
    ///
    /// Attaches to the environment exclusively, so it fails while the node is
    /// running. Edits are staged and applied by `commit()`.
    #[arg(long, default_value_t = false)]
    allow_writes: bool,

    /// Evaluate a single script string and exit instead of starting the REPL.
    #[arg(short = 'c', long, value_name = "SCRIPT", conflicts_with = "script")]
    command: Option<String>,

    /// Run a script file and exit instead of starting the REPL.
    #[arg(long, value_name = "FILE")]
    script: Option<PathBuf>,
}

/// Runs the console command.
pub(crate) fn run(args: DbconsoleArgs) -> eyre::Result<()> {
    let datadir = args
        .datadir
        .as_deref()
        .expect("main checks --datadir before running the console");
    let db = if args.allow_writes {
        ConsoleDb::attach_readwrite(datadir)?
    } else {
        ConsoleDb::attach_readonly(datadir)?
    };
    let mode = match db.mode() {
        AttachMode::ReadOnly => "ro",
        AttachMode::ReadWrite => "rw (exclusive)",
    };
    let envs = db.envs();
    let present: Vec<_> = envs.iter().filter(|e| e.present).map(|e| e.name).collect();
    let absent: Vec<_> = envs.iter().filter(|e| !e.present).map(|e| e.name).collect();

    // Ctrl-C sets the flag; the engine polls it between operations and ends
    // the current evaluation. Nothing in a native verb is cut short, so a
    // transaction in flight completes before the interrupt is seen.
    let interrupted = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, interrupted.clone())?;
    let mut session = session::Session::new(db, interrupted);
    recipes::install(&mut session)?;

    // The banner is chatter, not a result: it goes to stderr so that
    // `dbconsole -c '...'` in a script captures only the value.
    eprint!("attached {mode} · {}", present.join(", "));
    if !absent.is_empty() {
        eprint!(" (absent: {})", absent.join(", "));
    }
    eprintln!();
    if args.allow_writes {
        eprintln!("writes are staged; `commit()` applies them, `abort()` discards them");
    }

    let mut repl = repl::Repl::new(session);
    match (args.command, args.script) {
        (Some(script), _) => repl.eval_and_print(&script),
        (None, Some(file)) => repl.run_file(&file),
        (None, None) => repl.run(),
    }
}
