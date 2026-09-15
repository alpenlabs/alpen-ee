//! The rhai engine: the thin, sandboxed script shell over the trusted verbs.
//!
//! Each registered verb calls straight into [`ConsoleDb`], which owns the
//! codecs and the read transaction. Predicates passed to `scan_where` /
//! `count_where` are rhai closures invoked per row inside the native decode
//! loop, so only matched rows cross back into script (design doc §9).

use std::rc::Rc;

use alpen_ee_database::console::{ConsoleDb, Row};
use rhai::{Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, NativeCallContext};

use super::value::row_to_map;

/// Builds a rhai engine with the console verbs registered against `db`.
pub(crate) fn build(db: Rc<ConsoleDb>) -> Engine {
    let mut engine = Engine::new();
    // Sandbox rails: kill a runaway script rather than wedging the process.
    engine.set_max_operations(5_000_000);
    engine.set_max_call_levels(64);
    engine.set_max_expr_depths(128, 64);

    register_reads(&mut engine, db);
    register_write_stubs(&mut engine);
    engine
}

/// Registers the read verbs: `get`, `count_where`, `scan_where`.
fn register_reads(engine: &mut Engine, db: Rc<ConsoleDb>) {
    let get_db = db.clone();
    engine.register_fn(
        "get",
        move |table: ImmutableString,
              key: ImmutableString|
              -> Result<Dynamic, Box<EvalAltResult>> {
            match get_db.get(table.as_str(), key.as_str()).map_err(to_err)? {
                Some(row) => Ok(row_to_map(&row).into()),
                None => Ok(Dynamic::UNIT),
            }
        },
    );

    let count_db = db.clone();
    engine.register_fn(
        "count_where",
        move |ctx: NativeCallContext<'_>,
              table: ImmutableString,
              pred: FnPtr|
              -> Result<i64, Box<EvalAltResult>> {
            let mut callback = |row: &Row| call_pred(&ctx, &pred, row);
            let count = count_db
                .count_where(table.as_str(), &mut callback)
                .map_err(to_err)?;
            Ok(count as i64)
        },
    );

    let scan_db = db;
    engine.register_fn(
        "scan_where",
        move |ctx: NativeCallContext<'_>,
              table: ImmutableString,
              pred: FnPtr|
              -> Result<Dynamic, Box<EvalAltResult>> {
            let mut callback = |row: &Row| call_pred(&ctx, &pred, row);
            let rows = scan_db
                .scan(table.as_str(), &mut callback)
                .map_err(to_err)?;
            let array: Array = rows
                .iter()
                .map(|(_, row)| Dynamic::from_map(row_to_map(row)))
                .collect();
            Ok(array.into())
        },
    );
}

/// Registers the write verbs as stubs, so a script that reaches for them gets a
/// clear message rather than an "unknown function" error.
fn register_write_stubs(engine: &mut Engine) {
    const MSG: &str = "write verbs are not implemented in this console scaffold";
    engine.register_fn(
        "set",
        |_t: ImmutableString,
         _k: ImmutableString,
         _f: ImmutableString,
         _v: Dynamic|
         -> Result<(), Box<EvalAltResult>> { Err(MSG.into()) },
    );
    engine.register_fn(
        "del",
        |_t: ImmutableString, _k: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            Err(MSG.into())
        },
    );
    engine.register_fn("commit", || -> Result<(), Box<EvalAltResult>> {
        Err(MSG.into())
    });
    engine.register_fn("abort", || -> Result<(), Box<EvalAltResult>> {
        Err(MSG.into())
    });
}

/// Invokes a rhai predicate closure against one decoded row.
fn call_pred(ctx: &NativeCallContext<'_>, pred: &FnPtr, row: &Row) -> eyre::Result<bool> {
    let map = row_to_map(row);
    pred.call_within_context::<bool>(ctx, (map,))
        .map_err(|e| eyre::eyre!("predicate error: {e}"))
}

/// Maps an `eyre` error into a rhai runtime error.
fn to_err(err: eyre::Report) -> Box<EvalAltResult> {
    err.to_string().into()
}
