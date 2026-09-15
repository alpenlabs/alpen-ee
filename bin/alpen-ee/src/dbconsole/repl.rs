//! The REPL loop, meta-command handling, and result rendering.

use std::{
    io::{self, Write},
    rc::Rc,
};

use alpen_ee_database::console::ConsoleDb;
use rhai::{Dynamic, Engine, Scope};

use super::{engine, value::hex};

/// One console session: the attach handle plus a persistent script scope.
pub(crate) struct Session {
    db: Rc<ConsoleDb>,
    engine: Engine,
    scope: Scope<'static>,
}

impl Session {
    /// Builds a session over an attached [`ConsoleDb`].
    pub(crate) fn new(db: ConsoleDb) -> Self {
        let db = Rc::new(db);
        let engine = engine::build(db.clone());
        Self {
            db,
            engine,
            scope: Scope::new(),
        }
    }

    /// Runs the interactive prompt until EOF or `.quit`.
    pub(crate) fn run_repl(&mut self) {
        println!("type .help for meta-commands, .quit to exit");
        let stdin = io::stdin();
        loop {
            print!("db> ");
            let _ = io::stdout().flush();

            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => {
                    println!();
                    break;
                }
                Ok(_) => {}
                Err(err) => {
                    eprintln!("input error: {err}");
                    break;
                }
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('.') {
                if self.meta(rest) {
                    break;
                }
                continue;
            }
            if let Err(err) = self.eval_and_print(line) {
                eprintln!("error: {err}");
            }
        }
    }

    /// Handles a dot meta-command (already stripped of the leading `.`).
    /// Returns `true` if the session should exit.
    fn meta(&self, line: &str) -> bool {
        let mut parts = line.split_whitespace();
        match parts.next().unwrap_or("") {
            "quit" | "exit" => return true,
            "help" => print_help(),
            "tables" => self.print_tables(),
            "schema" => match parts.next() {
                Some(name) => self.print_schema(name),
                None => eprintln!("usage: .schema <table>"),
            },
            "staged" => {
                println!("no staged changes (write path not implemented in this scaffold)")
            }
            other => eprintln!("unknown meta-command .{other}; try .help"),
        }
        false
    }

    /// Prints the table inventory: name, row count, and restoration class.
    fn print_tables(&self) {
        println!("  {:<28} {:>8}  class", "table", "rows");
        for info in self.db.table_infos() {
            let count = match self.db.count(info.name) {
                Ok(rows) => rows.to_string(),
                Err(_) => "-".to_owned(),
            };
            println!("  {:<28} {:>8}  {}", info.name, count, info.class);
        }
    }

    /// Prints one table's schema description.
    fn print_schema(&self, name: &str) {
        match self.db.info(name) {
            Ok(info) => {
                println!("  table: {}", info.name);
                println!("  {}", info.class);
                println!("  key:   {}", info.key_desc);
                println!("  value: {}", info.value_desc);
            }
            Err(err) => eprintln!("error: {err}"),
        }
    }

    /// Evaluates one script fragment against the persistent scope and prints a
    /// non-unit result.
    pub(crate) fn eval_and_print(&mut self, src: &str) -> eyre::Result<()> {
        let result = self
            .engine
            .eval_with_scope::<Dynamic>(&mut self.scope, src)
            .map_err(|e| eyre::eyre!("{e}"))?;
        if !result.is_unit() {
            println!("{}", render(&result));
        }
        Ok(())
    }
}

/// Prints the meta-command help.
fn print_help() {
    println!("meta-commands:");
    println!("  .tables            list tables with row counts and restoration class");
    println!("  .schema <table>    show a table's key/value shape and class");
    println!("  .staged            show pending writes (write path not implemented)");
    println!("  .help              this help");
    println!("  .quit / .exit      leave the console");
    println!();
    println!("verbs (rhai expressions):");
    println!("  get(table, key)              fetch one decoded row by hex key");
    println!("  count_where(table, |r| ...)  count rows matching a predicate");
    println!("  scan_where(table, |r| ...)   list rows matching a predicate");
    println!();
    println!("example:");
    println!(r#"  count_where("ProverTaskSchema", |t| t.status == "PermanentFailure")"#);
}

/// Renders a rhai value for the prompt: maps as `{ k: v, ... }`, arrays with a
/// row count, blobs as hex.
fn render(value: &Dynamic) -> String {
    if value.is_array() {
        let array = value.clone().cast::<rhai::Array>();
        let mut out = format!("[{} rows]", array.len());
        for (index, item) in array.iter().enumerate() {
            out.push_str(&format!("\n  [{index}] {}", render(item)));
        }
        out
    } else if value.is_map() {
        let map = value.clone().cast::<rhai::Map>();
        let mut fields: Vec<String> = map
            .iter()
            .map(|(key, val)| format!("{key}: {}", render_scalar(val)))
            .collect();
        fields.sort();
        format!("{{ {} }}", fields.join(", "))
    } else {
        render_scalar(value)
    }
}

/// Renders a scalar rhai value (blobs as hex, unit as `null`).
fn render_scalar(value: &Dynamic) -> String {
    if value.is_blob() {
        format!("0x{}", hex(&value.clone().cast::<rhai::Blob>()))
    } else if value.is_unit() {
        "null".to_owned()
    } else {
        value.to_string()
    }
}
