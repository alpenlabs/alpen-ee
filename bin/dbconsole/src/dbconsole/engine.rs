//! The rhai engine: the thin, sandboxed script shell over the trusted verbs.
//!
//! Each registered verb calls straight into [`ConsoleDb`], which owns the
//! codecs and the read transaction. Predicates passed to `scan_where` /
//! `count_where` are rhai closures invoked per row inside the native decode
//! loop, so only matched rows cross back into script (design doc §9).

use std::{
    error::Error,
    fmt,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use alpen_ee_database::console::{ConsoleDb, Direction, Range, Record};
use rhai::{
    Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, NativeCallContext, Position,
};

use super::{
    handle::ValueHandle,
    value::{dynamic_to_field, field_to_dynamic, record_to_map},
};

/// Builds a rhai engine with the console verbs registered against `db`.
///
/// `interrupted` is polled between operations: once set, the evaluation in
/// progress ends with a termination error. That replaces an operation cap — a
/// legitimate loop over a large table has no natural bound, and a slow scan
/// wants stopping by hand, not by budget. The depth limits stay; they guard
/// the stack, not time.
pub(crate) fn build(db: Rc<ConsoleDb>, interrupted: Arc<AtomicBool>) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_call_levels(64);
    engine.set_max_expr_depths(128, 64);
    engine.on_progress(move |_ops| {
        if interrupted.load(Ordering::Relaxed) {
            Some(Dynamic::UNIT)
        } else {
            None
        }
    });

    register_value_handle(&mut engine);
    register_reads(&mut engine, db.clone());
    register_writes(&mut engine, db);
    engine
}

/// Registers the decoded-value handle scripts hold between a read and a write.
///
/// A value stays in its decoded form for the whole round trip. It is never
/// rebuilt out of rhai, because the script boundary is many-to-one — `()` could
/// be `None` or unit, a string could be a field or an enum variant, a blob could
/// be bytes or a large integer — so a value reconstructed from a script could
/// decode differently in the parts nobody edited. Editing the handle instead
/// keeps every untouched field exactly as it was read.
///
/// The handle remembers which table it came from, so each edit is checked
/// against that table's real type as it is made.
fn register_value_handle(engine: &mut Engine) {
    engine.register_type_with_name::<ValueHandle>("Value");

    engine.register_fn("get", |handle: &mut ValueHandle, field: ImmutableString| {
        handle
            .value
            .get(field.as_str())
            .map_or(Dynamic::UNIT, field_to_dynamic)
    });

    engine.register_indexer_get(|handle: &mut ValueHandle, field: ImmutableString| {
        handle
            .value
            .get(field.as_str())
            .map_or(Dynamic::UNIT, field_to_dynamic)
    });

    engine.register_fn(
        "set",
        |handle: &mut ValueHandle,
         field: ImmutableString,
         new: Dynamic|
         -> Result<(), Box<EvalAltResult>> { handle.set_field(field.as_str(), &new) },
    );

    engine.register_fn("table", |handle: &mut ValueHandle| handle.table.clone());
    engine.register_fn("to_string", |handle: &mut ValueHandle| {
        handle.value.to_string()
    });
}

/// Registers the read verbs.
///
/// Every walk runs natively with the predicate called per row; only what
/// matched crosses back, and it crosses once. The scan verbs come in pairs, with
/// and without a limit, because rhai has no optional arguments; and in
/// families by what they cover — the whole table, a key range, or a key
/// prefix — forward or from the top end.
fn register_reads(engine: &mut Engine, db: Rc<ConsoleDb>) {
    let get_db = db.clone();
    engine.register_fn(
        "get",
        move |table: ImmutableString,
              key: ImmutableString|
              -> Result<Dynamic, Box<EvalAltResult>> {
            match get_db.get(table.as_str(), key.as_str()).map_err(to_err)? {
                Some(record) => Ok(record_to_map(&record).into()),
                None => Ok(Dynamic::UNIT),
            }
        },
    );

    let count_db = db.clone();
    engine.register_fn(
        "count",
        move |table: ImmutableString| -> Result<i64, Box<EvalAltResult>> {
            Ok(count_db.count(table.as_str()).map_err(to_err)? as i64)
        },
    );

    let count_where_db = db.clone();
    engine.register_fn(
        "count_where",
        move |ctx: NativeCallContext<'_>,
              table: ImmutableString,
              pred: FnPtr|
              -> Result<i64, Box<EvalAltResult>> {
            let mut callback =
                |record: &Record| call_pred(&ctx, &pred, record_to_map(record).into());
            let count = count_where_db
                .count_where(table.as_str(), &mut callback)
                .map_err(to_err)?;
            Ok(count as i64)
        },
    );

    // Whole-table scans: scan_where(t, pred [, n]) and the same from the end.
    for (name, direction) in [
        ("scan_where", Direction::Forward),
        ("scan_rev_where", Direction::Backward),
    ] {
        let unlimited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  pred: FnPtr|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                scan_rows(
                    &unlimited,
                    &ctx,
                    &table,
                    &Range::All,
                    &pred,
                    direction,
                    None,
                )
            },
        );
        let limited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  pred: FnPtr,
                  limit: i64|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let limit = check_limit(limit)?;
                scan_rows(
                    &limited,
                    &ctx,
                    &table,
                    &Range::All,
                    &pred,
                    direction,
                    Some(limit),
                )
            },
        );
    }

    // Ranges on ordered keys: scan_range(t, from, to, pred [, n]), from..=to
    // written as `get` takes them.
    for (name, direction) in [
        ("scan_range", Direction::Forward),
        ("scan_rev_range", Direction::Backward),
    ] {
        let unlimited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  from: ImmutableString,
                  to: ImmutableString,
                  pred: FnPtr|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let range = between(&from, &to);
                scan_rows(&unlimited, &ctx, &table, &range, &pred, direction, None)
            },
        );
        let limited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  from: ImmutableString,
                  to: ImmutableString,
                  pred: FnPtr,
                  limit: i64|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let range = between(&from, &to);
                let limit = check_limit(limit)?;
                scan_rows(
                    &limited,
                    &ctx,
                    &table,
                    &range,
                    &pred,
                    direction,
                    Some(limit),
                )
            },
        );
    }

    // Prefixes on hex-shaped ordered keys: scan_prefix(t, hex, pred [, n]).
    for (name, direction) in [
        ("scan_prefix", Direction::Forward),
        ("scan_rev_prefix", Direction::Backward),
    ] {
        let unlimited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  prefix: ImmutableString,
                  pred: FnPtr|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let range = Range::Prefix(prefix.to_string());
                scan_rows(&unlimited, &ctx, &table, &range, &pred, direction, None)
            },
        );
        let limited = db.clone();
        engine.register_fn(
            name,
            move |ctx: NativeCallContext<'_>,
                  table: ImmutableString,
                  prefix: ImmutableString,
                  pred: FnPtr,
                  limit: i64|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                let range = Range::Prefix(prefix.to_string());
                let limit = check_limit(limit)?;
                scan_rows(
                    &limited,
                    &ctx,
                    &table,
                    &range,
                    &pred,
                    direction,
                    Some(limit),
                )
            },
        );
    }

    for (name, direction) in [("first", Direction::Forward), ("last", Direction::Backward)] {
        let edge_db = db.clone();
        engine.register_fn(
            name,
            move |table: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
                let mut found = Dynamic::UNIT;
                let mut visit = |record: &Record| {
                    found = record_to_map(record).into();
                    Ok(true)
                };
                edge_db
                    .scan(table.as_str(), &Range::All, direction, Some(1), &mut visit)
                    .map_err(to_err)?;
                Ok(found)
            },
        );
    }

    // Keys only: keys(t), keys_where(t, pred [, n]), keys_range(t, from, to),
    // keys_prefix(t, hex). None of these decodes a value.
    let keys_db = db.clone();
    engine.register_fn(
        "keys",
        move |table: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            scan_keys(&keys_db, None, &table, &Range::All, None)
        },
    );
    let keys_where_db = db.clone();
    engine.register_fn(
        "keys_where",
        move |ctx: NativeCallContext<'_>,
              table: ImmutableString,
              pred: FnPtr|
              -> Result<Dynamic, Box<EvalAltResult>> {
            scan_keys(
                &keys_where_db,
                Some((&ctx, &pred)),
                &table,
                &Range::All,
                None,
            )
        },
    );
    let keys_limited_db = db.clone();
    engine.register_fn(
        "keys_where",
        move |ctx: NativeCallContext<'_>,
              table: ImmutableString,
              pred: FnPtr,
              limit: i64|
              -> Result<Dynamic, Box<EvalAltResult>> {
            let limit = check_limit(limit)?;
            scan_keys(
                &keys_limited_db,
                Some((&ctx, &pred)),
                &table,
                &Range::All,
                Some(limit),
            )
        },
    );
    let keys_range_db = db.clone();
    engine.register_fn(
        "keys_range",
        move |table: ImmutableString,
              from: ImmutableString,
              to: ImmutableString|
              -> Result<Dynamic, Box<EvalAltResult>> {
            scan_keys(&keys_range_db, None, &table, &between(&from, &to), None)
        },
    );
    let keys_prefix_db = db;
    engine.register_fn(
        "keys_prefix",
        move |table: ImmutableString,
              prefix: ImmutableString|
              -> Result<Dynamic, Box<EvalAltResult>> {
            let range = Range::Prefix(prefix.to_string());
            scan_keys(&keys_prefix_db, None, &table, &range, None)
        },
    );
}

/// The inclusive range between two keys typed at the prompt.
fn between(from: &str, to: &str) -> Range {
    Range::Between {
        from: from.to_owned(),
        to: to.to_owned(),
    }
}

/// Walks the keys of `table` in `range`, in `direction`, returning the records
/// `pred` accepts as an array of `{ key, value }` maps, at most `limit` of them.
///
/// Each record is converted to a rhai map once. The predicate receives it
/// shared, and a match is unshared back into the result — the callee's copy is
/// gone by then, so that is a move, not a clone.
fn scan_rows(
    db: &ConsoleDb,
    ctx: &NativeCallContext<'_>,
    table: &str,
    range: &Range,
    pred: &FnPtr,
    direction: Direction,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut rows = Array::new();
    let mut visit = |record: &Record| {
        let map = Dynamic::from_map(record_to_map(record)).into_shared();
        let keep = call_pred(ctx, pred, map.clone())?;
        if keep {
            rows.push(map.flatten());
        }
        Ok(keep)
    };
    db.scan(table, range, direction, limit, &mut visit)
        .map_err(to_err)?;
    Ok(rows.into())
}

/// Walks the keys of `table` in `range` without decoding a value, returning
/// those `pred` accepts (or all of them) as an array of strings, at most
/// `limit`.
fn scan_keys(
    db: &ConsoleDb,
    pred: Option<(&NativeCallContext<'_>, &FnPtr)>,
    table: &str,
    range: &Range,
    limit: Option<usize>,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut keys = Array::new();
    let mut visit = |key: &str| {
        let keep = match pred {
            Some((ctx, pred)) => call_pred(ctx, pred, key.into())?,
            None => true,
        };
        if keep {
            keys.push(key.into());
        }
        Ok(keep)
    };
    db.keys(table, range, Direction::Forward, limit, &mut visit)
        .map_err(to_err)?;
    Ok(keys.into())
}

/// A limit typed at the prompt must be a positive count.
fn check_limit(limit: i64) -> Result<usize, Box<EvalAltResult>> {
    usize::try_from(limit)
        .ok()
        .filter(|limit| *limit > 0)
        .ok_or_else(|| format!("limit must be a positive number, got {limit}").into())
}

/// Registers the write verbs.
///
/// Every verb stages; nothing reaches the store until `commit`. A write is only
/// accepted once the table has been shown to survive a decode/encode round trip
/// — otherwise the parts of the record the write did not name could come back
/// changed.
fn register_writes(engine: &mut Engine, db: Rc<ConsoleDb>) {
    let del_db = db.clone();
    engine.register_fn(
        "del",
        move |table: ImmutableString, key: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            del_db
                .stage_delete(table.as_str(), key.as_str())
                .map_err(to_err)
        },
    );
    // del(table, [keys]): every key or none, presence checked in one read.
    let del_many_db = db.clone();
    engine.register_fn(
        "del",
        move |table: ImmutableString, keys: Array| -> Result<i64, Box<EvalAltResult>> {
            let keys: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    key.into_immutable_string()
                        .map(|s| s.to_string())
                        .map_err(|actual| {
                            format!("del expects a list of key strings, found {actual}")
                        })
                })
                .collect::<Result<_, _>>()?;
            let staged = del_many_db
                .stage_delete_many(table.as_str(), &keys)
                .map_err(to_err)?;
            Ok(staged as i64)
        },
    );

    // commit() returns the total applied; with more than one environment
    // it also says how many landed in each, since they land separately.
    let commit_db = db.clone();
    engine.register_fn("commit", move || -> Result<i64, Box<EvalAltResult>> {
        let report = commit_db.commit().map_err(to_err)?;
        if report.landed.len() > 1 {
            for (env, count) in &report.landed {
                println!("committed {count} edit(s) to `{env}`");
            }
        }
        Ok(report.total() as i64)
    });

    let abort_db = db.clone();
    engine.register_fn("abort", move || -> i64 { abort_db.abort() as i64 });

    let edit_db = db.clone();
    engine.register_fn(
        "edit",
        move |table: ImmutableString,
              key: ImmutableString|
              -> Result<ValueHandle, Box<EvalAltResult>> {
            let value = edit_db
                .read_value(table.as_str(), key.as_str())
                .map_err(to_err)?;
            // Hold the `env/Table` form, so a value written back under either
            // spelling of its table is recognised as going home.
            let table = edit_db.qualified_name(table.as_str()).map_err(to_err)?;
            Ok(ValueHandle::new(table, value, edit_db.clone()))
        },
    );

    let put_db = db.clone();
    engine.register_fn(
        "put",
        move |table: ImmutableString,
              key: ImmutableString,
              handle: ValueHandle|
              -> Result<(), Box<EvalAltResult>> {
            // A value belongs to the table it was decoded from: writing it into
            // another means writing one Rust type's shape where a different one
            // is expected. Say so, rather than letting it fail as a mismatch.
            let target = put_db.qualified_name(table.as_str()).map_err(to_err)?;
            if handle.table != target {
                return Err(format!(
                    "this value was read from `{}` and cannot be written to `{target}`",
                    handle.table
                )
                .into());
            }
            put_db
                .stage_put(table.as_str(), key.as_str(), handle.value)
                .map_err(to_err)
        },
    );

    let set_db = db;
    engine.register_fn(
        "set",
        move |table: ImmutableString,
              key: ImmutableString,
              field: ImmutableString,
              value: Dynamic|
              -> Result<(), Box<EvalAltResult>> {
            let value = dynamic_to_field(&value).map_err(|e| -> Box<EvalAltResult> { e.into() })?;
            set_db
                .stage_set(table.as_str(), key.as_str(), field.as_str(), value)
                .map_err(to_err)
        },
    );
}

/// An interrupt that fired inside a predicate, carried through the core's
/// error type so [`to_err`] can turn it back into the termination the session
/// recognises, rather than a runtime error that reads like a bug.
#[derive(Debug)]
struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted inside a predicate")
    }
}

impl Error for Interrupted {}

/// Invokes a rhai predicate closure on one argument, expecting a boolean.
///
/// An interrupt fires between operations, which includes the operations of
/// a predicate running inside a native scan; it has to end the scan too.
fn call_pred(ctx: &NativeCallContext<'_>, pred: &FnPtr, arg: Dynamic) -> eyre::Result<bool> {
    pred.call_within_context::<bool>(ctx, (arg,))
        .map_err(|e| match *e {
            EvalAltResult::ErrorTerminated(..) => eyre::Report::new(Interrupted),
            other => eyre::eyre!("predicate error: {other}"),
        })
}

/// Maps an `eyre` error into a rhai runtime error, or back into the
/// termination an interrupted predicate was carrying.
fn to_err(err: eyre::Report) -> Box<EvalAltResult> {
    if err.downcast_ref::<Interrupted>().is_some() {
        return Box::new(EvalAltResult::ErrorTerminated(
            Dynamic::UNIT,
            Position::NONE,
        ));
    }
    err.to_string().into()
}
