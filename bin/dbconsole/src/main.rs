//! `dbconsole` — an operator console over the Alpen EE store.
//!
//! An interactive prompt, a `-c` expression or a `--script` file over the EE
//! MDBX store, decoding values through the node's own codecs. The scripting
//! shell lives in [`dbconsole`]; the codec-owning core lives in
//! `alpen_ee_database::console`.

use std::process;

use clap::Parser;

mod dbconsole;

fn main() {
    // Rust ignores SIGPIPE, so `dbconsole ... | head` would panic on the first
    // write after the reader goes away; a CLI should just stop.
    // SAFETY: `signal` with `SIG_DFL` only restores the default disposition,
    // before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args = dbconsole::DbconsoleArgs::parse();
    // An operator wants the message, not a report with source locations.
    if let Err(err) = dbconsole::run(args) {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
