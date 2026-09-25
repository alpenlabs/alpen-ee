//! One console session: the attach, the engine, and what persists between
//! inputs.
//!
//! Two things outlive an input. Variables live in the [`Scope`], as rhai
//! provides. Functions live in the session's *library*: an [`AST`] holding
//! only function definitions, which every input is evaluated together with,
//! and which each input's own definitions are merged into afterwards, later
//! ones replacing earlier ones of the same name and arity. Without the
//! library a `fn` typed at the prompt would be gone on the next line, and a
//! recipe could not exist. Recipes and `.load`ed files enter the library the
//! same way an input does.
//!
//! An evaluation can be interrupted. The session owns a flag that a signal
//! handler sets; the engine's progress hook checks it between operations and
//! ends the evaluation with a distinct error, staged edits untouched. The
//! hook never fires inside a native verb, so an interrupt during `commit()`
//! lands after the transaction.

use std::{
    collections::BTreeSet,
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use alpen_ee_database::console::ConsoleDb;
use rhai::{Dynamic, Engine, EvalAltResult, ParseErrorType, Scope, AST};

use super::engine;

/// A function the session knows, for `.fns`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FnInfo {
    /// The function's name.
    pub(crate) name: String,
    /// Its parameter names, in order.
    pub(crate) params: Vec<String>,
    /// Its `///` doc comment lines, if any.
    pub(crate) docs: Vec<String>,
}

impl FnInfo {
    /// `name(a, b)`.
    pub(crate) fn signature(&self) -> String {
        format!("{}({})", self.name, self.params.join(", "))
    }
}

/// The attach handle plus everything that persists between inputs.
pub(crate) struct Session {
    db: Rc<ConsoleDb>,
    engine: Engine,
    scope: Scope<'static>,
    library: AST,
    /// Names of the functions that came from shipped recipes, for `.recipes`.
    recipes: BTreeSet<String>,
    interrupted: Arc<AtomicBool>,
}

impl Session {
    /// Builds a session over an attached [`ConsoleDb`].
    ///
    /// `interrupted` is the flag an interrupt handler sets; the session
    /// clears it before each evaluation and the engine polls it during one.
    pub(crate) fn new(db: ConsoleDb, interrupted: Arc<AtomicBool>) -> Self {
        let db = Rc::new(db);
        let engine = engine::build(db.clone(), interrupted.clone());
        Self {
            db,
            engine,
            scope: Scope::new(),
            library: AST::empty(),
            recipes: BTreeSet::new(),
            interrupted,
        }
    }

    /// The attach this session runs over.
    pub(crate) fn db(&self) -> &ConsoleDb {
        &self.db
    }

    /// Evaluates one input against the persistent scope and library.
    ///
    /// The input's function definitions are kept for later inputs whether or
    /// not its statements succeed: a definition is static, and losing it to a
    /// typo in the same input would be a surprise.
    pub(crate) fn eval(&mut self, src: &str) -> eyre::Result<Dynamic> {
        let ast = self
            .engine
            .compile_with_scope(&self.scope, src)
            .map_err(|e| eyre::eyre!("{e}"))?;
        self.run(ast)
    }

    /// Compiles and evaluates a file the same way as an input, so its
    /// functions join the library and its top-level statements run.
    pub(crate) fn load_file(&mut self, path: &Path) -> eyre::Result<Dynamic> {
        let ast = self
            .engine
            .compile_file(path.to_path_buf())
            .map_err(|e| eyre::eyre!("{e}"))?;
        self.run(ast)
    }

    /// Adds a shipped script's functions to the library as recipes, without
    /// running its statements.
    pub(crate) fn define_recipes(&mut self, name: &str, src: &str) -> eyre::Result<()> {
        let mut ast = self
            .engine
            .compile(src)
            .map_err(|e| eyre::eyre!("recipe file {name}: {e}"))?;
        ast.clear_statements();
        self.recipes.extend(
            ast.iter_functions()
                .filter(|f| !f.name.starts_with("anon$"))
                .map(|f| f.name.to_owned()),
        );
        self.library = self.library.merge(&ast);
        Ok(())
    }

    /// The shipped recipes, sorted by name.
    pub(crate) fn recipes(&self) -> Vec<FnInfo> {
        self.functions()
            .into_iter()
            .filter(|f| self.recipes.contains(&f.name))
            .collect()
    }

    fn run(&mut self, ast: AST) -> eyre::Result<Dynamic> {
        self.interrupted.store(false, Ordering::SeqCst);
        // The library carries no statements, so merging puts every known
        // function behind this input's statements.
        let program = self.library.merge(&ast);
        let result = self
            .engine
            .eval_ast_with_scope::<Dynamic>(&mut self.scope, &program);

        let mut definitions = ast;
        definitions.clear_statements();
        self.library = self.library.merge(&definitions);

        result.map_err(|err| match *err {
            EvalAltResult::ErrorTerminated(..) => {
                eyre::eyre!("interrupted; staged edits are kept, `.staged` shows them")
            }
            other => eyre::eyre!("{other}"),
        })
    }

    /// Whether `src` is a syntactically unfinished fragment — an open block
    /// or expression the parser ran out of input inside — as opposed to
    /// complete or wrong. The prompt keeps reading on `true`.
    ///
    /// The parser reports a missing closer either as "script is incomplete"
    /// or as "expecting `}`" positioned past the last character; both mean
    /// more input would help. A missing token *inside* the input is a
    /// mistake and is not continued.
    pub(crate) fn is_incomplete(engine: &Engine, src: &str) -> bool {
        let Err(err) = engine.compile(src) else {
            return false;
        };
        match err.err_type() {
            ParseErrorType::UnexpectedEOF => true,
            ParseErrorType::MissingToken(..) | ParseErrorType::MissingSymbol(..) => {
                let trimmed = src.trim_end();
                let lines = trimmed.lines().count().max(1);
                let last_len = trimmed.lines().last().map_or(0, str::len);
                let line = err.position().line().unwrap_or(0);
                let column = err.position().position().unwrap_or(0);
                line > lines || (line == lines && column > last_len)
            }
            _ => false,
        }
    }

    /// Every function the session knows, sorted by name.
    ///
    /// Closures compile to anonymous functions in the library; they are an
    /// implementation detail of the function that holds them and are not
    /// listed.
    pub(crate) fn functions(&self) -> Vec<FnInfo> {
        let mut functions: Vec<FnInfo> = self
            .library
            .iter_functions()
            .filter(|f| !f.name.starts_with("anon$"))
            .map(|f| FnInfo {
                name: f.name.to_owned(),
                params: f.params.iter().map(|p| (*p).to_owned()).collect(),
                // A doc block arrives as one string with its `///` lines
                // joined; split it back into lines without the markers.
                docs: f
                    .comments
                    .iter()
                    .flat_map(|block| block.lines())
                    .map(|line| {
                        line.trim()
                            .trim_start_matches("///")
                            .trim_start_matches("/**")
                            .trim_end_matches("*/")
                            .trim()
                            .to_owned()
                    })
                    .filter(|line| !line.is_empty())
                    .collect(),
            })
            .collect();
        functions.sort_by(|a, b| a.name.cmp(&b.name));
        functions
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use alpen_ee_database::test_db::TempDatadir;

    use super::*;

    /// A read-only session over a seeded datadir, which is returned with it
    /// and must outlive the session.
    fn scratch_session() -> (TempDatadir, Session) {
        let datadir = TempDatadir::seeded();
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        (datadir, Session::new(db, Arc::new(AtomicBool::new(false))))
    }

    #[test]
    fn a_function_defined_on_one_input_is_callable_on_the_next() {
        let (_datadir, mut session) = scratch_session();
        let _ = session.eval("fn twice(x) { x * 2 }").unwrap();
        assert_eq!(session.eval("twice(21)").unwrap().as_int().unwrap(), 42);

        // A redefinition replaces the earlier one.
        let _ = session.eval("fn twice(x) { x * 3 }").unwrap();
        assert_eq!(session.eval("twice(21)").unwrap().as_int().unwrap(), 63);
        assert_eq!(session.functions().len(), 1);
    }

    #[test]
    fn variables_persist_and_a_failed_input_keeps_its_functions() {
        let (_datadir, mut session) = scratch_session();
        let _ = session.eval("let base = 40;").unwrap();
        assert_eq!(session.eval("base + 2").unwrap().as_int().unwrap(), 42);

        // The statement fails, the definition stays.
        assert!(session.eval("fn add(a, b) { a + b }\nnope()").is_err());
        assert_eq!(session.eval("add(base, 2)").unwrap().as_int().unwrap(), 42);
    }

    #[test]
    fn an_unfinished_fragment_is_incomplete_and_a_wrong_one_is_not() {
        let engine = Engine::new();
        assert!(Session::is_incomplete(&engine, "fn f() {"));
        assert!(Session::is_incomplete(&engine, "for x in [1, 2] {\n  x"));
        assert!(Session::is_incomplete(&engine, "let v = ["));
        assert!(Session::is_incomplete(&engine, "fn f() {\n"));
        assert!(!Session::is_incomplete(&engine, "fn f() { 1 }"));
        assert!(!Session::is_incomplete(&engine, "let = 5"));
        assert!(!Session::is_incomplete(&engine, "1 +* 2"));
        assert!(!Session::is_incomplete(&engine, "fn f( { }"));
    }

    #[test]
    fn a_loaded_file_defines_functions_and_runs_its_statements() {
        let (datadir, mut session) = scratch_session();
        fs::create_dir_all(&datadir).unwrap();
        let file = datadir.join("helpers.rhai");
        fs::write(
            &file,
            "/// Doubles.\nfn twice(x) { x * 2 }\nlet loaded = true;\n",
        )
        .unwrap();

        let _ = session.load_file(&file).unwrap();
        assert_eq!(session.eval("twice(4)").unwrap().as_int().unwrap(), 8);
        assert!(session.eval("loaded").unwrap().as_bool().unwrap());

        let fns = session.functions();
        assert_eq!(fns[0].signature(), "twice(x)");
        assert_eq!(fns[0].docs, vec!["Doubles."]);

        // A closure inside a function is not a function of the session's.
        let _ = session.eval("fn keep(v) { v.filter(|x| x > 1) }").unwrap();
        let names: Vec<_> = session.functions().into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["keep", "twice"]);
    }

    #[test]
    fn an_interrupt_ends_an_evaluation_and_the_session_goes_on() {
        let (_datadir, mut session) = scratch_session();
        let flag = session.interrupted.clone();
        let raiser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::SeqCst);
        });
        let err = session.eval("let n = 0; loop { n += 1; }").unwrap_err();
        raiser.join().unwrap();
        assert!(err.to_string().contains("interrupted"), "{err}");

        // The flag is cleared for the next input, which runs normally.
        assert_eq!(session.eval("1 + 1").unwrap().as_int().unwrap(), 2);
    }
}
