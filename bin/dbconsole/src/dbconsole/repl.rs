//! The prompt: line editing, meta-commands, and result rendering.
//!
//! Evaluation belongs to the [`Session`]; this module reads inputs, decides
//! whether a line is a meta-command or script, and prints what comes back.
//! The editor keeps a history file under the user's data directory (never in
//! the node's datadir) and asks the parser whether an input is finished, so a
//! `fn` body or a `for` block spans lines without any prompt-side protocol.

use std::{
    fs,
    io::{self, BufRead, IsTerminal},
    mem,
    path::{Path, PathBuf},
};

use rhai::{Array, Blob, Dynamic, Engine, Map};
use rustyline::{
    error::ReadlineError,
    history::FileHistory,
    validate::{ValidationContext, ValidationResult, Validator},
    Completer, Config, Editor, Helper, Highlighter, Hinter,
};

use super::{
    session::{FnInfo, Session},
    value::hex,
};

/// The prompt shown for a new input.
const PROMPT: &str = "db> ";

/// Tells the editor when an input is still open, so Enter continues it.
///
/// Holds a bare engine for parsing only: syntax needs no registered verbs.
#[derive(Completer, Helper, Highlighter, Hinter)]
struct ConsoleHelper {
    parser: Engine,
}

impl Validator for ConsoleHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        // Meta-commands are one line by definition.
        if input.trim_start().starts_with('.') || !Session::is_incomplete(&self.parser, input) {
            Ok(ValidationResult::Valid(None))
        } else {
            Ok(ValidationResult::Incomplete)
        }
    }
}

/// The interactive loop over a session.
pub(crate) struct Repl {
    session: Session,
}

impl Repl {
    pub(crate) fn new(session: Session) -> Self {
        Self { session }
    }

    /// Runs the prompt until end of input or `.quit`.
    ///
    /// On a terminal that is the line editor; on a pipe it is a plain reader
    /// that still joins an unfinished input with the lines after it, so a
    /// scripted `printf ... | dbconsole` behaves the same.
    pub(crate) fn run(&mut self) -> eyre::Result<()> {
        println!("type .help for meta-commands, .quit to exit");
        if io::stdin().is_terminal() {
            self.run_editor()
        } else {
            self.run_piped()
        }
    }

    /// Reads inputs from a non-terminal stdin, no editing, no history.
    fn run_piped(&mut self) -> eyre::Result<()> {
        self.run_reader(io::stdin().lock())
    }

    /// Runs the inputs `reader` yields, one after another, the way a shell
    /// runs a script under `set -e`: the first input that fails ends the run
    /// with an error, so a `commit()` further down can never apply a batch
    /// that was only half staged. At the prompt the error is shown and the
    /// operator decides; here nobody is watching.
    fn run_reader(&mut self, reader: impl BufRead) -> eyre::Result<()> {
        let parser = Engine::new();
        let mut pending = String::new();
        let mut line_number = 0usize;
        for line in reader.lines() {
            let line = line?;
            line_number += 1;
            if pending.is_empty() && line.trim().is_empty() {
                continue;
            }
            pending.push_str(&line);
            pending.push('\n');
            if !pending.trim_start().starts_with('.') && Session::is_incomplete(&parser, &pending) {
                continue;
            }
            let input = mem::take(&mut pending);
            let input = input.trim();
            let outcome = match input.strip_prefix('.') {
                Some(rest) => self.meta(rest),
                None => self.eval_and_print(input).map(|()| false),
            };
            match outcome {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => eyre::bail!(
                    "line {line_number}: {err}\nstopped there; the rest of the input was not \
                     run and nothing staged was committed"
                ),
            }
        }
        if !pending.trim().is_empty() {
            eyre::bail!(
                "input ended inside an unfinished expression, which was not run; nothing \
                 staged was committed"
            );
        }
        Ok(())
    }

    /// Runs one input, meta-command or script. Returns `true` to exit.
    fn handle(&mut self, input: &str) -> bool {
        let outcome = match input.strip_prefix('.') {
            Some(rest) => self.meta(rest),
            None => self.eval_and_print(input).map(|()| false),
        };
        match outcome {
            Ok(exit) => exit,
            Err(err) => {
                eprintln!("error: {err}");
                false
            }
        }
    }

    fn run_editor(&mut self) -> eyre::Result<()> {
        let mut editor: Editor<ConsoleHelper, FileHistory> =
            Editor::with_config(Config::builder().auto_add_history(false).build())?;
        editor.set_helper(Some(ConsoleHelper {
            parser: Engine::new(),
        }));
        let history = history_path();
        if let Some(path) = &history {
            // A missing file is the first run; anything else is worth a note
            // but not worth refusing to start.
            if let Err(err) = editor.load_history(path) {
                if !matches!(err, ReadlineError::Io(ref io) if io.kind() == io::ErrorKind::NotFound)
                {
                    eprintln!("history not loaded from {}: {err}", path.display());
                }
            }
        }

        loop {
            let line = match editor.readline(PROMPT) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!("^C");
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(err) => {
                    eprintln!("input error: {err}");
                    break;
                }
            };
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let _ = editor.add_history_entry(input);
            if self.handle(input) {
                break;
            }
        }

        if let Some(path) = &history {
            if let Err(err) = fs::create_dir_all(path.parent().expect("history has a parent"))
                .map_err(ReadlineError::from)
                .and_then(|()| editor.save_history(path))
            {
                eprintln!("history not saved to {}: {err}", path.display());
            }
        }
        Ok(())
    }

    /// Runs a script file and prints its final value, for `--script`.
    pub(crate) fn run_file(&mut self, path: &Path) -> eyre::Result<()> {
        let result = self.session.load_file(path)?;
        print_result(&result);
        Ok(())
    }

    /// Evaluates one input and prints a non-unit result.
    pub(crate) fn eval_and_print(&mut self, src: &str) -> eyre::Result<()> {
        let result = self.session.eval(src)?;
        print_result(&result);
        Ok(())
    }

    /// Handles a dot meta-command (already stripped of the leading `.`).
    /// Returns `true` if the session should exit, and an error for a
    /// command that could not do its job, which a piped run treats like a
    /// failed evaluation.
    fn meta(&mut self, line: &str) -> eyre::Result<bool> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let rest = parts.next().map(str::trim).unwrap_or("");
        match command {
            "quit" | "exit" => return Ok(true),
            "help" => print_help(),
            "tables" => self.print_tables(),
            "schema" => {
                if rest.is_empty() {
                    eyre::bail!("usage: .schema <table>");
                }
                self.print_schema(rest)?;
            }
            "staged" => self.print_staged(rest == "full"),
            "fns" => self.print_functions(
                self.session.functions(),
                "no functions defined; define one with `fn name(args) {{ ... }}` or .load a file",
            ),
            "recipes" => {
                self.print_functions(self.session.recipes(), "no recipes shipped in this build")
            }
            "load" => {
                if rest.is_empty() {
                    eyre::bail!("usage: .load <file.rhai>");
                }
                let result = self.session.load_file(Path::new(rest))?;
                print_result(&result);
                println!("loaded {rest}");
            }
            other => eyre::bail!("unknown meta-command .{other}; try .help"),
        }
        Ok(false)
    }

    /// Prints functions with their doc comments, or `empty` if there are none.
    fn print_functions(&self, functions: Vec<FnInfo>, empty: &str) {
        if functions.is_empty() {
            println!("{empty}");
            return;
        }
        for info in functions {
            println!("  {}", info.signature());
            for line in info.docs {
                println!("      {line}");
            }
        }
    }

    /// Edits listed one by one before `.staged` summarises instead.
    const STAGED_LIST_LIMIT: usize = 20;

    /// Prints the edits waiting for a `commit()`: each one while there are
    /// few, a count per table otherwise, and every one on `.staged full`.
    fn print_staged(&self, full: bool) {
        let db = self.session.db();
        let staged = db.staged();
        if staged.is_empty() {
            println!("no staged changes");
            return;
        }
        println!(
            "{} staged change(s); `commit()` applies them:",
            staged.len()
        );
        if full || staged.len() <= Self::STAGED_LIST_LIMIT {
            for (index, op) in staged.iter().enumerate() {
                println!("  [{index}] {}", op.describe());
            }
            return;
        }
        for line in db.staged_summary() {
            println!(
                "  {}/{}: {} del, {} put",
                line.env, line.table, line.deletes, line.puts
            );
        }
        println!("  (`.staged full` lists every edit)");
    }

    /// Prints the table inventory, grouped by environment: name and row count.
    fn print_tables(&self) {
        let db = self.session.db();
        for env in db.envs() {
            let state = if env.present { "" } else { " (absent)" };
            println!("[{}]{state}", env.name);
            if env.tables.is_empty() {
                println!("  (no tables reflected yet)");
                continue;
            }
            println!("  {:<28} {:>8}", "table", "rows");
            for info in env.tables {
                let count = match db.count(&format!("{}/{}", env.name, info.name)) {
                    Ok(rows) => rows.to_string(),
                    Err(_) => "-".to_owned(),
                };
                println!("  {:<28} {:>8}", info.name, count);
            }
        }
    }

    /// Prints one table's schema description.
    fn print_schema(&self, name: &str) -> eyre::Result<()> {
        let info = self.session.db().info(name)?;
        println!("  table: {}", info.name);
        println!("  env:   {}", info.env);
        println!("  key:   {}", info.key_desc);
        println!("  value: {}", info.value_desc);
        Ok(())
    }
}

/// Where the prompt's history lives: the user's data directory, never the
/// node's datadir. `None` if the platform has no such directory.
fn history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("alpen-ee").join("dbconsole_history"))
}

/// Prints a non-unit result.
fn print_result(value: &Dynamic) {
    if !value.is_unit() {
        println!("{}", render(value));
    }
}

/// Prints the meta-command help.
fn print_help() {
    println!("meta-commands:");
    println!("  .tables            list tables by environment, with record counts");
    println!("  .schema <table>    show a table's key and value shape");
    println!("  .staged [full]     show writes waiting for a commit()");
    println!("  .fns               list the functions this session knows");
    println!("  .recipes           list the shipped recipes, with what they stage");
    println!("  .load <file>       run a .rhai file; its functions stay defined");
    println!("  .help              this help");
    println!("  .quit / .exit      leave the console");
    println!();
    println!("verbs (rhai expressions):");
    println!("  get(table, key)                    fetch one record by key");
    println!("  count(table)                       row count, O(1)");
    println!("  count_where(table, |r| ...)        count records matching a predicate");
    println!("  scan_where(table, |r| ... [, n])   matching records, at most n");
    println!("  scan_rev_where(table, |r| ... [, n]) the same from the end");
    println!("  first(table) / last(table)         one record, or () if empty");
    println!("  keys(table)                        every key, no values decoded");
    println!("  keys_where(table, |k| ... [, n])   keys matching a predicate");
    println!("  scan_range(table, from, to, |r| ... [, n])   keys from..=to (ordered keys)");
    println!("  scan_prefix(table, hex, |r| ... [, n])       keys starting with hex");
    println!("  scan_rev_range / scan_rev_prefix   the same from the top end");
    println!("  keys_range(table, from, to) / keys_prefix(table, hex)");
    println!();
    println!("a record is {{ key, value }}: `r.key` is where it lives,");
    println!("`r.value.<field>` is what it holds");
    println!();
    println!("a table is named bare (\"ProverTaskSchema\") or as env/Table");
    println!("(\"prover/ProverTaskSchema\"); the latter is required only if the");
    println!("bare name exists in more than one environment");
    println!();
    println!("scripting: `fn name(args) {{ ... }}` stays defined for the session;");
    println!("an open block continues on the next line; Ctrl-C stops a running");
    println!("evaluation and keeps staged edits");
    println!();
    println!("write verbs (need --allow-writes, which attaches exclusively):");
    println!("  edit(table, key)             read a value out to edit in hand");
    println!("  v.set(field, value)          change one field (checked immediately)");
    println!("  put(table, key, v)           stage a held value at a key (overwrites)");
    println!("  set(table, key, field, v)    stage a one-field edit in place");
    println!("  del(table, key)              stage a deletion");
    println!("  del(table, [keys])           stage many deletions, all or none");
    println!("  commit()                     apply staged writes in one transaction");
    println!("  abort()                      discard staged writes");
    println!();
    println!("example:");
    println!(r#"  count_where("ProverTaskSchema", |r| r.value.status == "PermanentFailure")"#);
}

/// Rows of a result array printed before the rest is summarised.
///
/// Display only: the array is intact, and a script sees every element. An
/// operator who wants more gives the scan a limit or binds the result.
const ROWS_SHOWN: usize = 100;

/// Renders a rhai value for the prompt: maps as `{ k: v, ... }`, arrays with a
/// row count and at most [`ROWS_SHOWN`] rows, blobs as hex.
fn render(value: &Dynamic) -> String {
    if value.is_array() {
        let array = value.clone().cast::<Array>();
        let mut out = format!("[{} rows]", array.len());
        for (index, item) in array.iter().take(ROWS_SHOWN).enumerate() {
            out.push_str(&format!("\n  [{index}] {}", render_value(item)));
        }
        if array.len() > ROWS_SHOWN {
            out.push_str(&format!(
                "\n  … {} more; bind the result or give the scan a limit",
                array.len() - ROWS_SHOWN
            ));
        }
        out
    } else {
        render_value(value)
    }
}

/// Renders one value, descending into nested rows and lists.
///
/// Reflected values nest (a struct field, an enum variant's payload), so this
/// recurses rather than deferring to rhai's own formatting — which would print
/// an inner blob in its debug form instead of as hex.
fn render_value(value: &Dynamic) -> String {
    if value.is_map() {
        let map = value.clone().cast::<Map>();
        let mut fields: Vec<String> = map
            .iter()
            .map(|(key, val)| format!("{key}: {}", render_value(val)))
            .collect();
        fields.sort();
        format!("{{ {} }}", fields.join(", "))
    } else if value.is_array() {
        let array = value.clone().cast::<Array>();
        if let Some(bytes) = as_byte_run(&array) {
            return format!("0x{}", hex(&bytes));
        }
        let items: Vec<String> = array.iter().map(render_value).collect();
        format!("[{}]", items.join(", "))
    } else {
        render_scalar(value)
    }
}

/// The shortest run of byte-valued integers shown as hex rather than as a list.
///
/// A `[u8; 32]` reaches serde as a sequence of integers with nothing marking it
/// as a byte string, and the row deliberately keeps it that way so it can be
/// written back unchanged. Prettifying it is therefore the renderer's job: a
/// guess here only affects what is printed, never what is stored or what a
/// predicate compares. The floor keeps genuinely small numeric tuples numeric.
const BYTE_RUN_FLOOR: usize = 16;

/// Recognises a list that reads better as a byte string.
fn as_byte_run(items: &[Dynamic]) -> Option<Vec<u8>> {
    if items.len() < BYTE_RUN_FLOOR {
        return None;
    }
    items
        .iter()
        .map(|item| item.as_int().ok().and_then(|v| u8::try_from(v).ok()))
        .collect()
}

/// Bytes shown in full before a blob is abbreviated to its ends and length.
///
/// A proof or a payload runs to megabytes; printing it whole would bury the
/// rest of the record. Display only: the value in the record is intact, and a
/// predicate sees every byte.
const BLOB_SHOW_FLOOR: usize = 64;

/// Renders a scalar rhai value (blobs as hex, unit as `null`).
fn render_scalar(value: &Dynamic) -> String {
    if value.is_blob() {
        let bytes = value.clone().cast::<Blob>();
        if bytes.len() <= BLOB_SHOW_FLOOR {
            format!("0x{}", hex(&bytes))
        } else {
            format!(
                "0x{}…{} ({} bytes)",
                hex(&bytes[..16]),
                hex(&bytes[bytes.len() - 8..]),
                bytes.len()
            )
        }
    } else if value.is_unit() {
        "null".to_owned()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{atomic::AtomicBool, Arc},
    };

    use alpen_ee_database::{console::ConsoleDb, test_db::TempDatadir};

    use super::*;
    use crate::dbconsole::recipes;

    #[test]
    fn a_piped_run_stops_at_the_first_error_and_commits_nothing() {
        let datadir = TempDatadir::seeded();
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let mut session = Session::new(db, Arc::new(AtomicBool::new(false)));
        recipes::install(&mut session).unwrap();
        let mut repl = Repl::new(session);

        let input = "let k = first(\"ProverTaskSchema\").key;\n\
                     set(\"ProverTaskSchema\", k, \"updated_at_secs\", 7)\n\
                     nope()\n\
                     commit()\n";
        let err = repl.run_reader(Cursor::new(input)).unwrap_err();
        assert!(err.to_string().starts_with("line 3:"), "{err}");

        // The edit is still only staged, and the store holds the old value.
        assert_eq!(repl.session.db().staged().len(), 1);
        let stored = repl
            .session
            .eval(
                "get(\"ProverTaskSchema\", first(\"ProverTaskSchema\").key).value.updated_at_secs",
            )
            .unwrap();
        assert_ne!(stored.as_int().unwrap(), 7);
    }

    /// A `.load` that stages edits and then fails is a failed input too:
    /// the run stops before a later `commit()`.
    #[test]
    fn a_piped_run_stops_when_a_loaded_file_fails() {
        let datadir = TempDatadir::seeded();
        let script = datadir.join("partial.rhai");
        fs::write(
            &script,
            "set(\"ProverTaskSchema\", first(\"ProverTaskSchema\").key, \"updated_at_secs\", 7);\nnope()\n",
        )
        .unwrap();
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let mut repl = Repl::new(Session::new(db, Arc::new(AtomicBool::new(false))));

        let input = format!(".load {}\ncommit()\n", script.display());
        let err = repl.run_reader(Cursor::new(input)).unwrap_err();
        assert!(err.to_string().starts_with("line 1:"), "{err}");
        assert_eq!(repl.session.db().staged().len(), 1);

        // So is an unknown meta-command, and input that ends mid-expression.
        let err = repl.run_reader(Cursor::new(".bogus\n")).unwrap_err();
        assert!(err.to_string().contains("unknown meta-command"), "{err}");
        let err = repl.run_reader(Cursor::new("commit(\n")).unwrap_err();
        assert!(err.to_string().contains("unfinished"), "{err}");
        assert_eq!(repl.session.db().staged().len(), 1);
    }

    #[test]
    fn a_piped_run_that_succeeds_runs_to_the_end() {
        let datadir = TempDatadir::seeded();
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        let mut repl = Repl::new(Session::new(db, Arc::new(AtomicBool::new(false))));
        let input = "fn twice(x) {\n  x * 2\n}\ntwice(2)\n.quit\nthis is never read\n";
        repl.run_reader(Cursor::new(input)).unwrap();
        assert_eq!(repl.session.functions().len(), 1);
    }
}
