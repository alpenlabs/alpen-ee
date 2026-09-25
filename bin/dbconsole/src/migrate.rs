//! The `migrate-sled` subcommand: fills a fresh MDBX store from the sled
//! store a previous binary wrote, offline.
//!
//! The mapping of trees to tables and the two value re-encodes live in the
//! console core (`alpen_ee_database::console::migrate`); this command reads
//! sled as raw bytes, hands them to the core's raw import, and then runs the
//! console's own checks over the result. The node's crates never see sled.

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};

use alpen_ee_database::{
    console::{
        migrate::{sled_trees, SledTree},
        ConsoleDb,
    },
    create_ee_envs,
};
use clap::Args;

use crate::dbconsole::{recipes, repl::render, session::Session};

/// sled's own default tree, present in every store and never ours.
const SLED_DEFAULT_TREE: &str = "__sled__default";

/// Arguments for `dbconsole migrate-sled`.
#[derive(Debug, Args)]
pub(crate) struct MigrateArgs {
    /// EE data directory: the one containing `sled/`, where `mdbx/` will be
    /// created.
    #[arg(long)]
    datadir: PathBuf,

    /// Rows written per MDBX transaction.
    #[arg(long, default_value_t = 4096)]
    batch: usize,

    /// Leave `sled/` under its own name afterwards instead of renaming it to
    /// `sled.migrated/`.
    #[arg(long, default_value_t = false)]
    keep_sled: bool,
}

/// Runs the migration end to end; any failure leaves `mdbx/` for inspection.
pub(crate) fn run(args: MigrateArgs) -> eyre::Result<()> {
    let sled_dir = args.datadir.join("sled");
    let mdbx_dir = args.datadir.join("mdbx");
    if !sled_dir.is_dir() {
        eyre::bail!("{} has no sled store to migrate", args.datadir.display());
    }
    if mdbx_dir.exists() {
        eyre::bail!(
            "{} already exists; this tool only fills a store it creates. Remove it to migrate again",
            mdbx_dir.display()
        );
    }

    let source = sled::open(&sled_dir)
        .map_err(|e| eyre::eyre!("cannot open the sled store at {}: {e}", sled_dir.display()))?;
    let present: BTreeSet<String> = source
        .tree_names()
        .into_iter()
        .map(|name| String::from_utf8_lossy(&name).into_owned())
        .filter(|name| name != SLED_DEFAULT_TREE)
        .collect();

    create_ee_envs(&args.datadir)?;
    let db = ConsoleDb::attach_readwrite(&args.datadir)?;

    println!("copying {} -> {}", sled_dir.display(), mdbx_dir.display());
    let mut copied = Vec::new();
    for entry in sled_trees() {
        if !present.contains(entry.tree) {
            println!(
                "  {:<28} absent in sled; {} left empty",
                entry.tree, entry.table
            );
            continue;
        }
        let rows = copy_tree(&source, &db, &entry, args.batch)?;
        copied.push((entry, rows));
    }
    let known: BTreeSet<&str> = sled_trees().iter().map(|t| t.tree).collect();
    for name in present.iter().filter(|name| !known.contains(name.as_str())) {
        println!("  {name:<28} UNKNOWN tree, left alone; report this");
    }

    println!("verifying");
    for (entry, rows) in &copied {
        let count = db.count(entry.table)?;
        if count != *rows {
            eyre::bail!(
                "`{}` holds {count} rows, but {rows} were copied from `{}`",
                entry.table,
                entry.tree
            );
        }
        let checked = db.check_table(entry.table)?;
        println!(
            "  {:<36} {checked:>8} rows decode and round-trip",
            entry.table
        );
    }

    let mut session = Session::new(db, Arc::new(AtomicBool::new(false)));
    recipes::install(&mut session)?;
    println!("summary of the migrated store");
    for recipe in [
        "chain_summary()",
        "batch_summary()",
        "prover_summary()",
        "broadcast_summary()",
    ] {
        let value = session.eval(recipe)?;
        println!("  {recipe:<20} {}", render(&value));
    }

    drop(source);
    if !args.keep_sled {
        let retired = args.datadir.join("sled.migrated");
        fs::rename(&sled_dir, &retired)?;
        println!(
            "sled store kept at {} until you delete it",
            retired.display()
        );
    }
    println!("done");
    Ok(())
}

/// Copies one tree into its table, in key order, batched.
fn copy_tree(
    source: &sled::Db,
    db: &ConsoleDb,
    entry: &SledTree,
    batch: usize,
) -> eyre::Result<usize> {
    let tree = source.open_tree(entry.tree)?;
    let started = Instant::now();
    let (key_rule, value_rule) = (entry.key, entry.value);
    let rows = tree.iter().map(move |item| {
        let (key, value) = item?;
        Ok((key_rule.apply(&key)?, value_rule.apply(&value)?))
    });
    let written = db.import_raw(entry.table, rows, batch)?;
    if written != tree.len() {
        eyre::bail!(
            "`{}` has {} rows but {written} were written to `{}`",
            entry.tree,
            tree.len(),
            entry.table
        );
    }
    println!(
        "  {:<28} -> {:<36} {written:>8} rows {} in {:.2}s",
        entry.tree,
        entry.table,
        value_rule.describe(),
        started.elapsed().as_secs_f64()
    );
    Ok(written)
}

#[cfg(test)]
mod tests {
    use std::{env, path::Path};

    use alpen_ee_database::{
        console::migrate::{to_sled_form, to_sled_key},
        test_db::TempDatadir,
    };

    use super::*;

    fn sled_dir(datadir: &Path) -> PathBuf {
        datadir.join("sled")
    }

    /// Builds a sled store in the old layout from a seeded MDBX store: every
    /// table's raw rows under the old tree name, the two re-encoded tables in
    /// their old form.
    fn sled_from(mdbx_datadir: &Path, into: &Path) {
        let db = ConsoleDb::attach_readonly(mdbx_datadir).unwrap();
        let sled = sled::open(sled_dir(into)).unwrap();
        for entry in sled_trees() {
            let tree = sled.open_tree(entry.tree).unwrap();
            let mut visit = |key: &[u8], value: &[u8]| {
                tree.insert(
                    to_sled_key(entry.table, key)?,
                    to_sled_form(entry.table, value)?,
                )?;
                Ok(())
            };
            db.walk_raw(entry.table, &mut visit).unwrap();
        }
        sled.flush().unwrap();
    }

    fn raw_rows(db: &ConsoleDb, table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut rows = Vec::new();
        let mut visit = |key: &[u8], value: &[u8]| {
            rows.push((key.to_vec(), value.to_vec()));
            Ok(())
        };
        db.walk_raw(table, &mut visit).unwrap();
        rows
    }

    /// Builds a sled store in the old layout from any MDBX datadir, so the
    /// tool can be run and timed on a real store:
    ///
    /// ```bash
    /// ALPEN_EE_SLED_FROM=<mdbx datadir> ALPEN_EE_SLED_TO=<new datadir> \
    ///     cargo test -p alpen-ee export_store_to_sled -- --ignored
    /// ./target/release/dbconsole migrate-sled --datadir <new datadir>
    /// ```
    #[test]
    #[ignore = "builds a sled store from a real datadir; run by hand"]
    fn export_store_to_sled() {
        let from = PathBuf::from(env::var("ALPEN_EE_SLED_FROM").expect("ALPEN_EE_SLED_FROM"));
        let to = PathBuf::from(env::var("ALPEN_EE_SLED_TO").expect("ALPEN_EE_SLED_TO"));
        sled_from(&from, &to);
    }

    #[test]
    fn a_seeded_store_round_trips_through_sled_byte_for_byte() {
        let original = TempDatadir::seeded();
        let migrated = TempDatadir::new();
        sled_from(&original, &migrated);

        run(MigrateArgs {
            datadir: migrated.to_path_buf(),
            batch: 3,
            keep_sled: false,
        })
        .unwrap();
        assert!(migrated.join("sled.migrated").is_dir());
        assert!(!sled_dir(&migrated).exists());

        let before = ConsoleDb::attach_readonly(&original).unwrap();
        let after = ConsoleDb::attach_readonly(&migrated).unwrap();
        for entry in sled_trees() {
            let expected = raw_rows(&before, entry.table);
            assert!(!expected.is_empty(), "{} not seeded", entry.table);
            assert_eq!(raw_rows(&after, entry.table), expected, "{}", entry.table);
        }
    }

    #[test]
    fn it_refuses_a_missing_sled_store_and_an_existing_mdbx_store() {
        let datadir = TempDatadir::new();
        let err = run(MigrateArgs {
            datadir: datadir.to_path_buf(),
            batch: 10,
            keep_sled: true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("no sled store"), "{err}");

        fs::create_dir_all(sled_dir(&datadir)).unwrap();
        fs::create_dir_all(datadir.join("mdbx")).unwrap();
        let err = run(MigrateArgs {
            datadir: datadir.to_path_buf(),
            batch: 10,
            keep_sled: true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    /// A prod store that predates a table has no tree for it: the table is
    /// created empty. A tree the mapping does not know is left where it is.
    #[test]
    fn an_absent_tree_leaves_its_table_empty_and_an_unknown_tree_is_kept() {
        let original = TempDatadir::seeded();
        let migrated = TempDatadir::new();
        sled_from(&original, &migrated);
        {
            let sled = sled::open(sled_dir(&migrated)).unwrap();
            sled.drop_tree("BcastL1TxNodeSchema").unwrap();
            sled.open_tree("SomeFutureSchema")
                .unwrap()
                .insert(b"k", b"v")
                .unwrap();
            sled.flush().unwrap();
        }

        run(MigrateArgs {
            datadir: migrated.to_path_buf(),
            batch: 100,
            keep_sled: true,
        })
        .unwrap();
        let after = ConsoleDb::attach_readonly(&migrated).unwrap();
        assert_eq!(after.count("da/L1BroadcastTxNodeSchema").unwrap(), 0);
        assert_eq!(after.count("da/L1BroadcastTxSchema").unwrap(), 1);
        let sled = sled::open(sled_dir(&migrated)).unwrap();
        assert!(sled.open_tree("SomeFutureSchema").unwrap().len() == 1);
    }

    /// The node refuses a datadir with sled and no MDBX, naming the command.
    #[test]
    fn the_node_refuses_to_start_on_an_unmigrated_datadir() {
        let datadir = TempDatadir::new();
        fs::create_dir_all(sled_dir(&datadir)).unwrap();
        let err = alpen_ee_database::open_ee_db(&datadir, 0).unwrap_err();
        assert!(err.to_string().contains("migrate-sled"), "{err}");
    }
}
