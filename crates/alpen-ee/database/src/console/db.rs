//! The console's attach handle over the EE MDBX store.
//!
//! [`ConsoleDb`] owns one [`MdbxEnv`] and the table registry, and scopes every
//! operation to a short read transaction via [`MdbxEnv::view`] — the read-txn
//! discipline the storage design mandates against a live writer.
//!
//! Scaffold status: attaches **read-only** to the prover environment only
//! (`<datadir>/mdbx/prover`), which is Class O and needs no Tier-1 substrate.
//! The write path, the node/DA/witness environments, and PID-lock liveness
//! negotiation are follow-ups.

use std::path::Path;

use alpen_db_store_mdbx::{MdbxConfig, MdbxEnv};

use super::{
    registry::{prover_env_tables, TableInfo, TableReflect},
    row::Row,
};

/// A read-only attach to the EE store's prover environment.
#[derive(Debug)]
pub struct ConsoleDb {
    env: MdbxEnv,
    tables: Vec<Box<dyn TableReflect>>,
}

impl ConsoleDb {
    /// Attaches read-only to the prover environment under
    /// `<datadir>/mdbx/prover`.
    ///
    /// Opens the environment without a write transaction, so it can attach
    /// alongside a running sequencer.
    pub fn attach_readonly(datadir: &Path) -> eyre::Result<Self> {
        let path = datadir.join("mdbx").join("prover");
        let env = MdbxEnv::open_readonly(&path, &MdbxConfig::default()).map_err(|e| {
            eyre::eyre!("failed to attach to prover env at {}: {e}", path.display())
        })?;
        Ok(Self {
            env,
            tables: prover_env_tables(),
        })
    }

    /// Returns the static metadata for every table, in registry order.
    pub fn table_infos(&self) -> Vec<TableInfo> {
        self.tables.iter().map(|table| table.info()).collect()
    }

    /// Looks up a table's reflection by name.
    fn table(&self, name: &str) -> eyre::Result<&dyn TableReflect> {
        self.tables
            .iter()
            .find(|table| table.info().name == name)
            .map(|table| table.as_ref())
            .ok_or_else(|| eyre::eyre!("unknown table `{name}`"))
    }

    /// Returns a table's metadata by name.
    pub fn info(&self, name: &str) -> eyre::Result<TableInfo> {
        Ok(self.table(name)?.info())
    }

    /// Counts the entries in a table.
    pub fn count(&self, name: &str) -> eyre::Result<usize> {
        let table = self.table(name)?;
        self.env.view(|reader| table.count(reader))
    }

    /// Fetches one decoded row by textual key.
    pub fn get(&self, name: &str, key: &str) -> eyre::Result<Option<Row>> {
        let table = self.table(name)?;
        self.env.view(|reader| table.get(reader, key))
    }

    /// Scans a table, returning `(key-hex, row)` for rows matching `pred`.
    pub fn scan(
        &self,
        name: &str,
        pred: &mut dyn FnMut(&Row) -> eyre::Result<bool>,
    ) -> eyre::Result<Vec<(String, Row)>> {
        let table = self.table(name)?;
        self.env.view(|reader| table.scan(reader, pred))
    }

    /// Counts the rows in a table matching `pred`.
    pub fn count_where(
        &self,
        name: &str,
        pred: &mut dyn FnMut(&Row) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        Ok(self.scan(name, pred)?.len())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicU32, Ordering},
    };

    use alpen_db_store_mdbx::{MdbxConfig, MdbxEnv, TableSpec};
    use strata_paas::{TaskRecordData, TaskStatus};

    use super::{
        super::row::{FieldValue, Row},
        ConsoleDb,
    };
    use crate::mdbxdb::ProverTaskSchema;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_datadir() -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("alpen-ee-console-{}-{unique}", process::id()))
    }

    /// Creates a prover env with two tasks: a pending one and a permanently
    /// failed one. The env handle is dropped before returning so the console can
    /// attach read-only in the same process.
    fn seed_prover_env(datadir: &Path) {
        let prover = datadir.join("mdbx").join("prover");
        let env = MdbxEnv::open(
            &prover,
            &MdbxConfig::default(),
            &[TableSpec::of::<ProverTaskSchema>()],
        )
        .unwrap();
        env.update(|writer| {
            writer.put::<ProverTaskSchema>(
                &vec![1u8, 2, 3],
                &TaskRecordData::new(TaskStatus::Pending),
            )?;
            writer.put::<ProverTaskSchema>(
                &vec![4u8, 5, 6],
                &TaskRecordData::new(TaskStatus::PermanentFailure {
                    error: "boom".to_owned(),
                }),
            )?;
            Ok::<_, alpen_db_store_mdbx::DbError>(())
        })
        .unwrap();
    }

    #[test]
    fn attaches_and_reflects_prover_tasks() {
        let datadir = temp_datadir();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        // Inventory + count go through MDBX stat.
        assert!(db
            .table_infos()
            .iter()
            .any(|info| info.name == "ProverTaskSchema"));
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 2);

        // get() decodes the value through the production codec and flattens the
        // status enum into scalar fields.
        let row = db.get("ProverTaskSchema", "040506").unwrap().unwrap();
        match row.get("status") {
            Some(FieldValue::Enum(status)) => assert_eq!(status, "PermanentFailure"),
            other => panic!("unexpected status field: {other:?}"),
        }

        // scan() pushes the predicate into the native decode loop.
        let mut pending = |row: &Row| {
            Ok(matches!(row.get("status"), Some(FieldValue::Enum(s)) if s == "Pending"))
        };
        let matched = db.scan("ProverTaskSchema", &mut pending).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, "010203");
        assert_eq!(db.count_where("ProverTaskSchema", &mut pending).unwrap(), 1);
    }
}
