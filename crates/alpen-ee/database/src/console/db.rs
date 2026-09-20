//! The console's attach handle over the EE MDBX store.
//!
//! [`ConsoleDb`] owns every environment under `<datadir>/mdbx` — `node`,
//! `prover`, `witness`, `da` — each with its own [`MdbxEnv`] and table registry,
//! and scopes every operation to a short read transaction via [`MdbxEnv::view`]
//! — the read-txn discipline the storage design mandates against a live
//! writer. A table name resolves to the environment it lives in, so a script
//! never names an environment unless a table name is ambiguous.
//!
//! An environment whose directory is missing is attached as *absent*: a full
//! node's datadir has only `node`, and a sequencer that never proved has no
//! receipts yet. Its tables are listed but any access to them says so.
//!
//! Writes are staged rather than applied as they are typed: an MDBX
//! environment admits one writer at a time, so holding a write transaction open
//! across a prompt would stall whoever else wants it. Edits accumulate in
//! memory and [`ConsoleDb::commit`] applies them in one short transaction per
//! environment, which is what makes a batch atomic *within* an environment.
//! MDBX has no transaction spanning environments, so a batch that touches two
//! lands in two steps; [`ConsoleDb::commit`] says which landed if the second
//! fails.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem,
    path::Path,
};

use alpen_store_mdbx::{Direction, MdbxConfig, MdbxEnv};

use super::{
    registry::{ee_envs, EnvSpec, Range, TableInfo, TableReflect, KEY_FIELD},
    value::{FieldValue, Record},
};

/// How the console attached, and therefore what it may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// Read-only: safe alongside a running node.
    ReadOnly,
    /// Read-write, held exclusively: only possible with the node down.
    ReadWrite,
}

/// One staged, not-yet-applied edit.
#[derive(Clone, Debug, PartialEq)]
pub enum StagedOp {
    /// Remove the record at this key.
    Delete {
        /// The environment the table lives in.
        env: &'static str,
        /// The table the record lives in.
        table: &'static str,
        /// The record's key, as typed.
        key: String,
    },
    /// Replace the record at this key with an edited copy of itself.
    ///
    /// The record is carried whole rather than as a field patch, because its
    /// value came from decoding what is stored: every field except the edited
    /// one holds exactly what was read back, so re-encoding cannot disturb them.
    Put {
        /// The environment the table lives in.
        env: &'static str,
        /// The table the record lives in.
        table: &'static str,
        /// What the edit changed, for `.staged`.
        change: String,
        /// The full edited record.
        record: Record,
    },
}

impl StagedOp {
    /// The environment this edit touches.
    pub fn env(&self) -> &'static str {
        match self {
            Self::Delete { env, .. } | Self::Put { env, .. } => env,
        }
    }

    /// The table this edit touches.
    pub fn table(&self) -> &'static str {
        match self {
            Self::Delete { table, .. } | Self::Put { table, .. } => table,
        }
    }

    /// The key this edit touches, as typed.
    pub fn key(&self) -> &str {
        match self {
            Self::Delete { key, .. } => key,
            Self::Put { record, .. } => &record.key,
        }
    }

    /// `del` or `put`, for a message.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Delete { .. } => "del",
            Self::Put { .. } => "put",
        }
    }

    /// A one-line rendering for `.staged`.
    pub fn describe(&self) -> String {
        match self {
            Self::Delete { env, table, key } => format!("del {env}/{table} {key}"),
            Self::Put {
                env,
                table,
                change,
                record,
            } => format!(
                "put {env}/{table} {} ({change})\n      {}",
                record.key, record.value
            ),
        }
    }
}

/// What a commit applied: each environment that landed, in order, with its
/// edit count. Environments land in separate transactions, so the report is
/// per environment rather than one number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    /// Environments applied, in the order they landed.
    pub landed: Vec<(&'static str, usize)>,
}

impl CommitReport {
    /// Edits applied across every environment.
    pub fn total(&self) -> usize {
        self.landed.iter().map(|(_, count)| count).sum()
    }
}

/// The staged edits to one table, counted by kind, for a summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedSummary {
    /// The environment the table lives in.
    pub env: &'static str,
    /// The table.
    pub table: &'static str,
    /// Deletes staged against it.
    pub deletes: usize,
    /// Puts (whole values or field edits) staged against it.
    pub puts: usize,
}

/// The staged edits, with an index of the keys they touch.
///
/// The index is what keeps the one-edit-per-key check constant per edit; a
/// recipe stages tens of thousands, and checking each against a list would
/// make the batch quadratic.
#[derive(Debug, Default)]
struct Batch {
    ops: Vec<StagedOp>,
    /// Per `(env, table)`, the keys with a staged edit and each edit's kind.
    /// Nested so a check probes with a borrowed key and allocates nothing.
    index: HashMap<(&'static str, &'static str), HashMap<String, &'static str>>,
}

impl Batch {
    fn push(&mut self, op: StagedOp) {
        self.index
            .entry((op.env(), op.table()))
            .or_default()
            .insert(op.key().to_owned(), op.kind());
        self.ops.push(op);
    }

    /// The kind of the edit already staged for this key, if any.
    fn prior(&self, env: &'static str, table: &'static str, key: &str) -> Option<&'static str> {
        self.index.get(&(env, table))?.get(key).copied()
    }

    fn clear(&mut self) {
        self.ops.clear();
        self.index.clear();
    }
}

/// One environment as the console sees it: attached, or absent from the datadir.
#[derive(Clone, Debug)]
pub struct EnvStatus {
    /// The environment's name (`node`, `prover`, `witness`, `da`).
    pub name: &'static str,
    /// Whether its directory existed and was opened.
    pub present: bool,
    /// The tables the console reflects in it, in registry order.
    pub tables: Vec<TableInfo>,
}

/// One environment's attach state.
#[derive(Debug)]
struct AttachedEnv {
    name: &'static str,
    /// `None` when the directory is missing: the environment is listed but
    /// every access to its tables is refused with a message saying so.
    env: Option<MdbxEnv>,
    tables: Vec<Box<dyn TableReflect>>,
}

impl AttachedEnv {
    fn status(&self) -> EnvStatus {
        EnvStatus {
            name: self.name,
            present: self.env.is_some(),
            tables: self.tables.iter().map(|table| table.info()).collect(),
        }
    }
}

/// A table name resolved to where it lives.
struct Resolved<'a> {
    env_name: &'static str,
    env: &'a MdbxEnv,
    table: &'a dyn TableReflect,
}

/// An attach to the EE store's environments.
#[derive(Debug)]
pub struct ConsoleDb {
    envs: Vec<AttachedEnv>,
    mode: AttachMode,
    staged: RefCell<Batch>,
}

impl ConsoleDb {
    /// Attaches read-only to every environment present under
    /// `<datadir>/mdbx`.
    ///
    /// Opens each environment without a write transaction, so it can attach
    /// alongside a running sequencer.
    pub fn attach_readonly(datadir: &Path) -> eyre::Result<Self> {
        Self::attach(datadir, AttachMode::ReadOnly, ee_envs())
    }

    /// Attaches read-write to every environment present under
    /// `<datadir>/mdbx`, each held exclusively.
    ///
    /// The exclusive open is the liveness guard: it fails outright while any
    /// other process holds an environment, so this cannot race a running node.
    /// A node keeps state in memory that the store alone does not capture, so
    /// an outside writer working against a live node would either be clobbered
    /// by its next flush or never observed at all.
    pub fn attach_readwrite(datadir: &Path) -> eyre::Result<Self> {
        Self::attach(datadir, AttachMode::ReadWrite, ee_envs())
    }

    /// Attaches to the environments in `specs`, skipping those whose directory
    /// is missing. Fails if none is present, since that means `datadir` is not
    /// an EE datadir at all.
    pub(crate) fn attach(
        datadir: &Path,
        mode: AttachMode,
        specs: Vec<EnvSpec>,
    ) -> eyre::Result<Self> {
        let mdbx_dir = datadir.join("mdbx");
        let mut envs = Vec::with_capacity(specs.len());
        for spec in specs {
            let path = mdbx_dir.join(spec.name);
            let env = if path.is_dir() {
                Some(open_env(&path, spec.name, mode)?)
            } else {
                None
            };
            envs.push(AttachedEnv {
                name: spec.name,
                env,
                tables: (spec.tables)(),
            });
        }
        if envs.iter().all(|attached| attached.env.is_none()) {
            eyre::bail!(
                "no EE environment under {}; --datadir must be the directory that contains `mdbx/`",
                mdbx_dir.display()
            );
        }
        Ok(Self {
            envs,
            mode,
            staged: RefCell::new(Batch::default()),
        })
    }

    /// How this console attached.
    pub fn mode(&self) -> AttachMode {
        self.mode
    }

    /// Every environment, present or absent, in registry order.
    pub fn envs(&self) -> Vec<EnvStatus> {
        self.envs.iter().map(AttachedEnv::status).collect()
    }

    /// Returns the static metadata for every table, in registry order.
    pub fn table_infos(&self) -> Vec<TableInfo> {
        self.envs
            .iter()
            .flat_map(|attached| attached.tables.iter().map(|table| table.info()))
            .collect()
    }

    /// Resolves a table name to the environment it lives in.
    ///
    /// A bare name resolves while it is unique across environments; `env/Table`
    /// names one explicitly, and is required if the bare name is ambiguous. A
    /// table in an absent environment is found but refused, so the message can
    /// say the environment is missing rather than that the table is unknown.
    fn table(&self, name: &str) -> eyre::Result<Resolved<'_>> {
        let (env_filter, table_name) = match name.split_once('/') {
            Some((env, table)) => (Some(env), table),
            None => (None, name),
        };
        if let Some(env) = env_filter {
            if !self.envs.iter().any(|attached| attached.name == env) {
                let known: Vec<_> = self.envs.iter().map(|attached| attached.name).collect();
                eyre::bail!(
                    "unknown environment `{env}` in `{name}`; environments are {}",
                    known.join(", ")
                );
            }
        }

        let mut hits = self
            .envs
            .iter()
            .filter(|attached| env_filter.is_none_or(|env| env == attached.name))
            .filter_map(|attached| {
                attached
                    .tables
                    .iter()
                    .find(|table| table.info().name == table_name)
                    .map(|table| (attached, table.as_ref()))
            });

        let Some((attached, table)) = hits.next() else {
            eyre::bail!("unknown table `{name}`");
        };
        if let Some((other, _)) = hits.next() {
            eyre::bail!(
                "`{table_name}` exists in more than one environment; name it as \
                 `{}/{table_name}` or `{}/{table_name}`",
                attached.name,
                other.name
            );
        }
        let Some(env) = &attached.env else {
            eyre::bail!(
                "`{table_name}` lives in the `{}` environment, which is not present under this datadir",
                attached.name
            );
        };
        Ok(Resolved {
            env_name: attached.name,
            env,
            table,
        })
    }

    /// Looks up an attached environment by name.
    ///
    /// Only called for names that a staged edit already resolved through
    /// [`Self::table`], so the environment is known and present.
    fn attached_env(&self, env_name: &str) -> &MdbxEnv {
        self.envs
            .iter()
            .find(|attached| attached.name == env_name)
            .and_then(|attached| attached.env.as_ref())
            .expect("a staged edit names an attached environment")
    }

    /// Returns a table's metadata by name.
    pub fn info(&self, name: &str) -> eyre::Result<TableInfo> {
        Ok(self.table(name)?.table.info())
    }

    /// Returns the `env/Table` form of a table name, however it was written.
    ///
    /// Two spellings of one table compare equal in this form, which is what a
    /// shell needs to check that a value is being written back where it came
    /// from.
    pub fn qualified_name(&self, name: &str) -> eyre::Result<String> {
        let resolved = self.table(name)?;
        Ok(format!(
            "{}/{}",
            resolved.env_name,
            resolved.table.info().name
        ))
    }

    /// Counts the entries in a table.
    pub fn count(&self, name: &str) -> eyre::Result<usize> {
        let resolved = self.table(name)?;
        resolved.env.view(|reader| resolved.table.count(reader))
    }

    /// Fetches one decoded record by textual key.
    pub fn get(&self, name: &str, key: &str) -> eyre::Result<Option<Record>> {
        let resolved = self.table(name)?;
        resolved.env.view(|reader| resolved.table.get(reader, key))
    }

    /// Walks the keys of a table in `range`, in `direction`, handing each
    /// decoded record to `visit`, which says whether it matched. Stops after
    /// `limit` matches, and returns how many there were.
    ///
    /// Nothing is collected here: the visitor keeps what it wants, so a count
    /// keeps nothing and a scan converts each match once, into the form its
    /// caller needs. A bounded range is refused on a table whose key encoding
    /// does not preserve order.
    pub fn scan(
        &self,
        name: &str,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&Record) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        let resolved = self.table(name)?;
        resolved
            .env
            .view(|reader| resolved.table.scan(reader, range, direction, limit, visit))
    }

    /// Counts the records in a table matching `pred`, keeping none of them.
    pub fn count_where(
        &self,
        name: &str,
        pred: &mut dyn FnMut(&Record) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        self.scan(name, &Range::All, Direction::Forward, None, pred)
    }

    /// Walks the keys of a table in `range`, in `direction`, without decoding
    /// a value, handing each rendered key to `visit`. Stops after `limit`
    /// matches, and returns how many there were.
    pub fn keys(
        &self,
        name: &str,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        visit: &mut dyn FnMut(&str) -> eyre::Result<bool>,
    ) -> eyre::Result<usize> {
        let resolved = self.table(name)?;
        resolved
            .env
            .view(|reader| resolved.table.keys(reader, range, direction, limit, visit))
    }

    // --- Staged writes ----------------------------------------------------

    /// Refuses a write this attach cannot accept.
    fn check_writable(&self) -> eyre::Result<()> {
        if self.mode != AttachMode::ReadWrite {
            eyre::bail!(
                "attached read-only; restart with --allow-writes (and the node stopped) to edit"
            );
        }
        Ok(())
    }

    /// Refuses an edit to a key that already has one staged.
    ///
    /// Two edits to one key would apply in staging order, which is easy to
    /// get wrong and, for two deletes, fails the whole environment's batch at
    /// commit. Naming the earlier edit at the prompt is cheaper than either.
    fn check_unstaged(
        &self,
        env: &'static str,
        table: &'static str,
        key: &str,
    ) -> eyre::Result<()> {
        if let Some(kind) = self.staged.borrow().prior(env, table, key) {
            eyre::bail!(
                "`{table}` key {key} already has a staged {kind}; commit() or abort() first"
            );
        }
        Ok(())
    }

    /// Stages a delete, validating the table, the key, and the record's presence
    /// now rather than at commit.
    pub fn stage_delete(&self, name: &str, key: &str) -> eyre::Result<()> {
        self.stage_delete_many(name, &[key.to_owned()]).map(|_| ())
    }

    /// Stages a delete of every key in `keys`, all or none, in one read
    /// transaction for the presence checks. Returns how many were staged.
    ///
    /// Presence is worth checking up front because the attach is exclusive: no
    /// one else can add the record between staging and commit, so "absent now"
    /// means the key is wrong. A key listed twice, or already staged, is
    /// refused before anything is queued, so a failed call stages nothing.
    pub fn stage_delete_many(&self, name: &str, keys: &[String]) -> eyre::Result<usize> {
        let resolved = self.table(name)?;
        let info = resolved.table.info();
        self.check_writable()?;

        // Keys are staged in their canonical spelling, so two spellings of one
        // key meet in the checks. A set for the list itself and the batch's
        // index for what is already staged keep a hundred thousand keys linear.
        let keys: Vec<String> = keys
            .iter()
            .map(|key| resolved.table.canonical_key(key))
            .collect::<eyre::Result<_>>()?;
        let mut listed: HashSet<&str> = HashSet::with_capacity(keys.len());
        for key in &keys {
            if !listed.insert(key) {
                eyre::bail!("key {key} is listed twice");
            }
            self.check_unstaged(resolved.env_name, info.name, key)?;
        }

        // Presence only: the value is never decoded, which matters for a
        // table whose values are megabytes.
        resolved.env.view(|reader| {
            for key in &keys {
                if !resolved.table.contains(reader, key)? {
                    eyre::bail!("`{}` has no record at key {key}", info.name);
                }
            }
            Ok(())
        })?;

        let count = keys.len();
        let mut staged = self.staged.borrow_mut();
        for key in keys {
            staged.push(StagedOp::Delete {
                env: resolved.env_name,
                table: info.name,
                key,
            });
        }
        Ok(count)
    }

    /// Reads and decodes the value stored at `key`, for editing in hand.
    ///
    /// Returned without its key so it can be written back at the same key or a
    /// different one; the value itself says nothing about where it lives.
    pub fn read_value(&self, name: &str, key: &str) -> eyre::Result<FieldValue> {
        let resolved = self.table(name)?;
        let info = resolved.table.info();
        Ok(resolved
            .env
            .view(|reader| resolved.table.get(reader, key))?
            .ok_or_else(|| eyre::eyre!("`{}` has no record at key {key}", info.name))?
            .value)
    }

    /// Converts a value to the exact form `name`'s decoder produces, failing if
    /// the table's type cannot account for it.
    ///
    /// Exposed so an editor can validate a change the moment it is made rather
    /// than when the write is staged: the error then lands on the edit that
    /// caused it.
    pub fn canonicalize(&self, name: &str, value: &FieldValue) -> eyre::Result<FieldValue> {
        self.table(name)?.table.canonicalize(value)
    }

    /// Stages a whole value to be written at `key`, replacing whatever is there.
    ///
    /// The value must be one this table produced — read with [`Self::read_value`]
    /// and edited in place — so that the parts of it nobody touched still hold
    /// exactly what was decoded. It is checked for faithful round-tripping
    /// before being queued, so a table whose reflector is lossy is refused
    /// rather than storing something that reads back differently.
    pub fn stage_put(&self, name: &str, key: &str, value: FieldValue) -> eyre::Result<()> {
        let resolved = self.table(name)?;
        let info = resolved.table.info();
        self.check_writable()?;
        let key = resolved.table.canonical_key(key)?;
        let value = resolved.table.canonicalize(&value)?;

        let exists = resolved
            .env
            .view(|reader| resolved.table.contains(reader, &key))?;
        let change = if exists { "overwrite" } else { "create" }.to_owned();

        self.check_unstaged(resolved.env_name, info.name, &key)?;
        self.staged.borrow_mut().push(StagedOp::Put {
            env: resolved.env_name,
            table: info.name,
            change,
            record: Record::new(key, value),
        });
        Ok(())
    }

    /// Stages a single field edit.
    ///
    /// A convenience over [`Self::read_value`] plus [`Self::stage_put`]: the
    /// record is read, the one field is replaced, and the whole value is queued.
    /// Every other field keeps the value it decoded to, so re-encoding cannot
    /// disturb them.
    pub fn stage_set(
        &self,
        name: &str,
        key: &str,
        field: &str,
        value: FieldValue,
    ) -> eyre::Result<()> {
        let resolved = self.table(name)?;
        let info = resolved.table.info();
        self.check_writable()?;
        let key = resolved.table.canonical_key(key)?;

        let mut decoded = self.read_value(name, &key)?;
        if !decoded.replace_field(field, value.clone()) {
            if field == KEY_FIELD {
                eyre::bail!(
                    "`{field}` addresses the record rather than being part of it; \
                     use put() to write a value at a different key"
                );
            }
            eyre::bail!("`{}` values have no field `{field}`", info.name);
        }

        // Canonicalising proves the edit encodes before it is queued, so a bad
        // value is reported at the prompt rather than failing the batch.
        let decoded = resolved
            .table
            .canonicalize(&decoded)
            .map_err(|e| eyre::eyre!("`{field}` cannot hold that value: {e}"))?;

        self.check_unstaged(resolved.env_name, info.name, &key)?;
        self.staged.borrow_mut().push(StagedOp::Put {
            env: resolved.env_name,
            table: info.name,
            change: format!("{field}={value}"),
            record: Record::new(key, decoded),
        });
        Ok(())
    }

    /// The edits waiting to be applied, in the order they were staged.
    pub fn staged(&self) -> Vec<StagedOp> {
        self.staged.borrow().ops.clone()
    }

    /// The staged edits counted by table, in the order tables were first
    /// touched.
    pub fn staged_summary(&self) -> Vec<StagedSummary> {
        let mut summary: Vec<StagedSummary> = Vec::new();
        for op in &self.staged.borrow().ops {
            let entry = match summary
                .iter_mut()
                .find(|line| line.env == op.env() && line.table == op.table())
            {
                Some(entry) => entry,
                None => {
                    summary.push(StagedSummary {
                        env: op.env(),
                        table: op.table(),
                        deletes: 0,
                        puts: 0,
                    });
                    summary.last_mut().expect("just pushed")
                }
            };
            match op {
                StagedOp::Delete { .. } => entry.deletes += 1,
                StagedOp::Put { .. } => entry.puts += 1,
            }
        }
        summary
    }

    /// Discards every staged edit, leaving the store untouched.
    pub fn abort(&self) -> usize {
        let mut staged = self.staged.borrow_mut();
        let discarded = staged.ops.len();
        staged.clear();
        discarded
    }

    /// Applies every staged edit, one transaction per environment, then clears
    /// the batch.
    ///
    /// Within an environment all of its edits land or none do: a failure
    /// part-way aborts that transaction. Environments are applied in the order
    /// they were first staged, and an environment's edits leave the batch as
    /// soon as they land — so if a later environment fails, what is still
    /// staged is exactly what has not been applied, and the error says what
    /// did. MDBX offers no transaction across environments, so this is the
    /// strongest guarantee available; a batch that needs to be all-or-nothing
    /// should stay within one environment.
    pub fn commit(&self) -> eyre::Result<CommitReport> {
        if self.mode != AttachMode::ReadWrite {
            eyre::bail!("attached read-only; nothing can be committed");
        }
        // Take the batch rather than clone it: a staged put carries its whole
        // value. What does not land goes back.
        let batch = mem::take(&mut *self.staged.borrow_mut());
        if batch.ops.is_empty() {
            return Ok(CommitReport::default());
        }

        // Group by environment, keeping each edit's order within its own.
        let mut by_env: Vec<(&'static str, Vec<StagedOp>)> = Vec::new();
        for op in batch.ops {
            match by_env.iter_mut().find(|(env, _)| *env == op.env()) {
                Some((_, ops)) => ops.push(op),
                None => by_env.push((op.env(), vec![op])),
            }
        }

        let mut report = CommitReport::default();
        let mut groups = by_env.into_iter();
        while let Some((env_name, ops)) = groups.next() {
            let env = self.attached_env(env_name);
            let result = env.update(|writer| -> eyre::Result<usize> {
                // Each table is resolved once per group, not once per edit.
                let mut tables: HashMap<&'static str, &dyn TableReflect> = HashMap::new();
                for op in &ops {
                    let table = match tables.get(op.table()) {
                        Some(table) => *table,
                        None => {
                            let table = self.table(&format!("{env_name}/{}", op.table()))?.table;
                            tables.insert(op.table(), table);
                            table
                        }
                    };
                    match op {
                        StagedOp::Delete { table: name, key, .. } => {
                            if !table.delete(writer, key)? {
                                eyre::bail!(
                                    "`{name}` has no record at key {key}; nothing in `{env_name}` was committed"
                                );
                            }
                        }
                        StagedOp::Put { record, .. } => table.put(writer, record)?,
                    }
                }
                Ok(ops.len())
            });

            match result {
                Ok(count) => report.landed.push((env_name, count)),
                Err(err) => {
                    // This environment's edits and every later one's are still
                    // unapplied: put them back, in the order they were.
                    let mut staged = self.staged.borrow_mut();
                    for op in ops.into_iter().chain(groups.flat_map(|(_, ops)| ops)) {
                        staged.push(op);
                    }
                    if report.landed.is_empty() {
                        return Err(err);
                    }
                    let landed: Vec<_> = report.landed.iter().map(|(env, _)| *env).collect();
                    return Err(eyre::eyre!(
                        "{err}\nedits to `{}` had already landed; the edits still staged are the ones not applied",
                        landed.join("`, `")
                    ));
                }
            }
        }

        Ok(report)
    }
}

/// Opens one environment in the posture `mode` calls for.
fn open_env(path: &Path, name: &str, mode: AttachMode) -> eyre::Result<MdbxEnv> {
    match mode {
        AttachMode::ReadOnly => MdbxEnv::open_readonly(path, &MdbxConfig::default()).map_err(|e| {
            eyre::eyre!(
                "failed to attach to the `{name}` environment at {}: {e}",
                path.display()
            )
        }),
        AttachMode::ReadWrite => MdbxEnv::open_readwrite_exclusive(path, &MdbxConfig::default())
            .map_err(|e| {
                eyre::eyre!(
                    "failed to attach read-write to the `{name}` environment at {}: {e}\n\
                     stop the node before attaching for writes",
                    path.display()
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use alpen_store_mdbx::{DbError, Direction, MdbxConfig, MdbxEnv, TableSpec};
    use strata_acct_types::Hash;
    use strata_paas::{TaskRecordData, TaskStatus};

    use super::{
        super::{
            registry::{heights_test_tables, prover_env_tables, EnvSpec, Range},
            value::FieldValue,
            Record,
        },
        AttachMode, ConsoleDb, StagedSummary,
    };
    use crate::{
        mdbxdb::{ExecBlockFinalizedSchema, ProverTaskSchema},
        test_db::TempDatadir,
    };

    /// Creates a prover env with two tasks: a pending one and a permanently
    /// failed one. The env handle is dropped before returning so the console can
    /// attach read-only in the same process.
    fn seed_prover_env(datadir: &Path) {
        seed_env(datadir, "prover");
    }

    /// Seeds the same two-task table into the environment directory `name`.
    ///
    /// The schema is only a sub-database name, so the prover task table can
    /// stand in for any environment's table when the point is routing.
    fn seed_env(datadir: &Path, name: &str) {
        let path = datadir.join("mdbx").join(name);
        let env = MdbxEnv::open(
            &path,
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
            Ok::<_, DbError>(())
        })
        .unwrap();
    }

    #[test]
    fn attaches_and_reflects_prover_tasks() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        // Inventory + count go through MDBX stat.
        assert!(db
            .table_infos()
            .iter()
            .any(|info| info.name == "ProverTaskSchema"));
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 2);

        // get() decodes through the production codec and reflects the value in
        // its serde shape: a data-carrying variant nests under its own name,
        // rather than being flattened into a scalar.
        let record = db.get("ProverTaskSchema", "040506").unwrap().unwrap();
        match record.value.get("status") {
            Some(FieldValue::Variant { name, .. }) => assert_eq!(name, "PermanentFailure"),
            other => panic!("unexpected status field: {other:?}"),
        }

        // The key belongs to the record, not to the value it holds.
        assert_eq!(record.key, "040506");
        assert!(
            record.value.get("key").is_none(),
            "the key leaked into the value's fields"
        );

        // scan() pushes the predicate into the native decode loop.
        let pending = |record: &Record| matches!(record.value.get("status"), Some(FieldValue::Enum(s)) if s == "Pending");
        let matched = collect(
            &db,
            "ProverTaskSchema",
            &Range::All,
            Direction::Forward,
            None,
            pending,
        );
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].key, "010203");
        assert_eq!(
            db.count_where("ProverTaskSchema", &mut |r| Ok(pending(r)))
                .unwrap(),
            1
        );
    }

    /// Collects the records `keep` accepts, the way a shell would.
    fn collect(
        db: &ConsoleDb,
        table: &str,
        range: &Range,
        direction: Direction,
        limit: Option<usize>,
        keep: impl Fn(&Record) -> bool,
    ) -> Vec<Record> {
        let mut rows = Vec::new();
        let mut visit = |record: &Record| {
            let matched = keep(record);
            if matched {
                rows.push(record.clone());
            }
            Ok(matched)
        };
        db.scan(table, range, direction, limit, &mut visit).unwrap();
        rows
    }

    /// The keys a scan of `table` visits, in order.
    fn keys_of(db: &ConsoleDb, table: &str, range: &Range, direction: Direction) -> Vec<String> {
        collect(db, table, range, direction, None, |_| true)
            .into_iter()
            .map(|r| r.key)
            .collect()
    }

    #[test]
    fn a_scan_walks_in_either_direction_and_stops_at_its_limit() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        let forward = collect(
            &db,
            "ProverTaskSchema",
            &Range::All,
            Direction::Forward,
            None,
            |_| true,
        );
        assert_eq!(
            forward.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["010203", "040506"]
        );

        let backward = collect(
            &db,
            "ProverTaskSchema",
            &Range::All,
            Direction::Backward,
            None,
            |_| true,
        );
        assert_eq!(
            backward.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["040506", "010203"]
        );

        // The limit counts matches, not rows visited, and ends the walk.
        let mut visited = 0;
        let mut first = |_: &Record| {
            visited += 1;
            Ok(true)
        };
        assert_eq!(
            db.scan(
                "ProverTaskSchema",
                &Range::All,
                Direction::Backward,
                Some(1),
                &mut first
            )
            .unwrap(),
            1
        );
        assert_eq!(visited, 1);
    }

    /// A visitor's error ends the walk and comes back as itself.
    #[test]
    fn a_visitor_error_ends_the_scan_and_is_returned() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        let mut visits = 0;
        let mut failing = |_: &Record| {
            visits += 1;
            Err(eyre::eyre!("predicate exploded"))
        };
        let err = db
            .scan(
                "ProverTaskSchema",
                &Range::All,
                Direction::Forward,
                None,
                &mut failing,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "predicate exploded");
        assert_eq!(visits, 1);
    }

    #[test]
    fn a_keys_walk_renders_every_key_without_a_value() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        let mut keys = Vec::new();
        let mut visit = |key: &str| {
            keys.push(key.to_owned());
            Ok(key.starts_with("04"))
        };
        let matched = db
            .keys(
                "ProverTaskSchema",
                &Range::All,
                Direction::Forward,
                None,
                &mut visit,
            )
            .unwrap();
        assert_eq!(keys, vec!["010203", "040506"]);
        assert_eq!(matched, 1);
    }

    // --- Staged writes ----------------------------------------------------

    /// A read-only attach must refuse an edit outright, not stage it and fail
    /// later at commit.
    #[test]
    fn a_read_only_attach_refuses_to_stage() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        let err = db.stage_delete("ProverTaskSchema", "010203").unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        assert!(db.staged().is_empty());
    }

    #[test]
    fn staging_rejects_an_unknown_table_and_a_bad_key() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        assert!(db.stage_delete("NoSuchSchema", "010203").is_err());

        let err = db.stage_delete("ProverTaskSchema", "zz").unwrap_err();
        assert!(err.to_string().contains("hex"), "{err}");
        assert!(db.staged().is_empty());
    }

    /// The attach is exclusive, so a record that is absent when staged cannot
    /// appear before commit: the key is simply wrong, and saying so at the
    /// prompt beats failing the whole batch later.
    #[test]
    fn staging_rejects_a_key_that_is_not_present() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let err = db.stage_delete("ProverTaskSchema", "0a0b0c").unwrap_err();
        assert!(err.to_string().contains("no record at key"), "{err}");
    }

    /// Nothing reaches the store until commit, and abort leaves it untouched.
    #[test]
    fn staged_deletes_only_land_on_commit() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 2);

        db.stage_delete("ProverTaskSchema", "010203").unwrap();
        assert_eq!(db.staged().len(), 1);
        assert_eq!(
            db.count("ProverTaskSchema").unwrap(),
            2,
            "staging touched the store"
        );

        assert_eq!(db.abort(), 1);
        assert!(db.staged().is_empty());
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 2);

        db.stage_delete("ProverTaskSchema", "010203").unwrap();
        assert_eq!(db.commit().unwrap().total(), 1);
        assert!(db.staged().is_empty());
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 1);
        assert!(db.get("ProverTaskSchema", "010203").unwrap().is_none());
        assert!(db.get("ProverTaskSchema", "040506").unwrap().is_some());
    }

    #[test]
    fn a_batch_commits_every_staged_edit_at_once() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        db.stage_delete("ProverTaskSchema", "010203").unwrap();
        db.stage_delete("ProverTaskSchema", "040506").unwrap();

        assert_eq!(db.commit().unwrap().total(), 2);
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 0);
    }

    #[test]
    fn committing_nothing_is_not_an_error() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        assert_eq!(db.commit().unwrap().total(), 0);
    }

    /// The exclusive attach is what keeps a console write off a live store, so
    /// it must fail while anything else holds the environment.
    #[test]
    fn a_read_write_attach_is_refused_while_the_env_is_held() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let holder = ConsoleDb::attach_readonly(&datadir).unwrap();
        let err = ConsoleDb::attach_readwrite(&datadir).unwrap_err();
        assert!(err.to_string().contains("stop the node"), "{err}");
        drop(holder);

        ConsoleDb::attach_readwrite(&datadir).expect("attach after the holder went away");
    }

    /// A field edit must change the named field and nothing else.
    #[test]
    fn a_staged_set_changes_only_the_named_field() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let before = db.get("ProverTaskSchema", "010203").unwrap().unwrap().value;

        db.stage_set(
            "ProverTaskSchema",
            "010203",
            "updated_at_secs",
            FieldValue::U64(1234),
        )
        .unwrap();
        assert_eq!(db.staged().len(), 1);
        assert_eq!(
            db.get("ProverTaskSchema", "010203").unwrap().unwrap().value,
            before,
            "staging touched the store"
        );

        assert_eq!(db.commit().unwrap().total(), 1);

        let after = db.get("ProverTaskSchema", "010203").unwrap().unwrap().value;
        assert_eq!(after.get("updated_at_secs"), Some(&FieldValue::U64(1234)));
        for (name, value) in before.fields().expect("struct-shaped") {
            if name != "updated_at_secs" {
                assert_eq!(after.get(name), Some(value), "`{name}` changed");
            }
        }
    }

    #[test]
    fn a_set_rejects_an_unknown_field_and_a_bad_value() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();

        let err = db
            .stage_set("ProverTaskSchema", "010203", "nope", FieldValue::U64(1))
            .unwrap_err();
        assert!(err.to_string().contains("no field `nope`"), "{err}");

        let err = db
            .stage_set(
                "ProverTaskSchema",
                "010203",
                "updated_at_secs",
                FieldValue::Str("not a number".to_owned()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot hold that value"), "{err}");
        assert!(db.staged().is_empty());
    }

    #[test]
    fn a_read_only_attach_refuses_a_set() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        let err = db
            .stage_set(
                "ProverTaskSchema",
                "010203",
                "updated_at_secs",
                FieldValue::U64(1),
            )
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    /// Deletes and edits in one batch land together or not at all.
    #[test]
    fn a_mixed_batch_commits_atomically() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        db.stage_set(
            "ProverTaskSchema",
            "010203",
            "updated_at_secs",
            FieldValue::U64(7),
        )
        .unwrap();
        db.stage_delete("ProverTaskSchema", "040506").unwrap();

        assert_eq!(db.commit().unwrap().total(), 2);
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 1);
        assert_eq!(
            db.get("ProverTaskSchema", "010203")
                .unwrap()
                .unwrap()
                .value
                .get("updated_at_secs"),
            Some(&FieldValue::U64(7))
        );
    }

    // --- Whole-value writes -----------------------------------------------

    /// Read a value out, change it in hand, write it back — the flow `set`
    /// shortcuts. Fields the edit did not name keep exactly what they decoded
    /// to.
    #[test]
    fn a_value_read_out_can_be_edited_and_put_back() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let before = db.read_value("ProverTaskSchema", "010203").unwrap();

        let mut edited = before.clone();
        assert!(edited.replace_field("updated_at_secs", FieldValue::U64(11)));
        assert!(edited.replace_field("retry_after_secs", FieldValue::U64(22)));
        db.stage_put("ProverTaskSchema", "010203", edited).unwrap();
        assert_eq!(db.commit().unwrap().total(), 1);

        let after = db.read_value("ProverTaskSchema", "010203").unwrap();
        assert_eq!(after.get("updated_at_secs"), Some(&FieldValue::U64(11)));
        assert_eq!(after.get("retry_after_secs"), Some(&FieldValue::U64(22)));
        assert_eq!(after.get("status"), before.get("status"));
        assert_eq!(after.get("metadata"), before.get("metadata"));
    }

    /// A value carries no key, so the same one can be written somewhere else.
    #[test]
    fn a_value_can_be_put_at_a_different_key() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let value = db.read_value("ProverTaskSchema", "010203").unwrap();

        db.stage_put("ProverTaskSchema", "aabbcc", value.clone())
            .unwrap();
        assert_eq!(db.commit().unwrap().total(), 1);

        assert_eq!(db.count("ProverTaskSchema").unwrap(), 3);
        assert_eq!(db.read_value("ProverTaskSchema", "aabbcc").unwrap(), value);
        // the source is untouched
        assert_eq!(db.read_value("ProverTaskSchema", "010203").unwrap(), value);
    }

    /// Overwriting is allowed, and the staged line says which it is.
    #[test]
    fn a_put_over_an_existing_key_overwrites() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let value = db.read_value("ProverTaskSchema", "010203").unwrap();

        db.stage_put("ProverTaskSchema", "040506", value.clone())
            .unwrap();
        assert!(db.staged()[0].describe().contains("overwrite"));
        assert_eq!(db.commit().unwrap().total(), 1);

        assert_eq!(
            db.count("ProverTaskSchema").unwrap(),
            2,
            "a record was added"
        );
        assert_eq!(db.read_value("ProverTaskSchema", "040506").unwrap(), value);

        db.stage_put("ProverTaskSchema", "ffeedd", value).unwrap();
        assert!(db.staged()[0].describe().contains("create"));
    }

    /// A fieldless variant can be named by a plain string, because a prompt has
    /// no enum literal — and what gets stored is the canonical variant.
    #[test]
    fn an_enum_field_accepts_its_variant_name_as_a_string() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        db.stage_set(
            "ProverTaskSchema",
            "010203",
            "status",
            FieldValue::Str("Completed".to_owned()),
        )
        .unwrap();
        assert_eq!(db.commit().unwrap().total(), 1);

        let after = db.read_value("ProverTaskSchema", "010203").unwrap();
        assert_eq!(
            after.get("status"),
            Some(&FieldValue::Enum("Completed".to_owned())),
            "the loose input was stored instead of the canonical form"
        );
    }

    /// A name that is not a variant is refused rather than stored as a string.
    #[test]
    fn an_unknown_variant_name_is_refused() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let err = db
            .stage_set(
                "ProverTaskSchema",
                "010203",
                "status",
                FieldValue::Str("NotAVariant".to_owned()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("NotAVariant"), "{err}");
        assert!(db.staged().is_empty());
    }

    #[test]
    fn a_read_only_attach_refuses_a_put() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        let value = db.read_value("ProverTaskSchema", "010203").unwrap();
        let err = db
            .stage_put("ProverTaskSchema", "010203", value)
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    // --- Environments -------------------------------------------------------

    /// A registry in which the prover task table lives in two environments,
    /// so routing and the `env/Table` qualification get exercised.
    fn two_env_specs() -> Vec<EnvSpec> {
        vec![
            EnvSpec {
                name: "node",
                tables: prover_env_tables,
            },
            EnvSpec {
                name: "prover",
                tables: prover_env_tables,
            },
        ]
    }

    /// A datadir with only some environments is normal — a full node has no
    /// prover store — so an absent one is listed, not fatal.
    #[test]
    fn an_absent_environment_is_listed_and_its_tables_are_refused() {
        let datadir = TempDatadir::new();
        seed_env(&datadir, "prover");

        let db = ConsoleDb::attach(&datadir, AttachMode::ReadOnly, two_env_specs()).unwrap();
        let envs = db.envs();
        assert_eq!(
            envs.iter().map(|e| (e.name, e.present)).collect::<Vec<_>>(),
            vec![("node", false), ("prover", true)]
        );
        assert!(
            !envs[0].tables.is_empty(),
            "an absent env still lists its tables"
        );

        let err = db.count("node/ProverTaskSchema").unwrap_err();
        assert!(err.to_string().contains("not present"), "{err}");
        assert_eq!(db.count("prover/ProverTaskSchema").unwrap(), 2);
    }

    /// The production registry attaches a prover-only datadir and resolves the
    /// bare table name, since it is unique there.
    #[test]
    fn a_prover_only_datadir_attaches_with_the_other_environments_absent() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);

        let db = ConsoleDb::attach_readonly(&datadir).unwrap();
        let present: Vec<_> = db
            .envs()
            .iter()
            .filter(|e| e.present)
            .map(|e| e.name)
            .collect();
        assert_eq!(present, vec!["prover"]);
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 2);
        assert_eq!(db.count("prover/ProverTaskSchema").unwrap(), 2);
    }

    #[test]
    fn a_datadir_with_no_environment_is_refused() {
        let datadir = TempDatadir::new();
        let err = ConsoleDb::attach_readonly(&datadir).unwrap_err();
        assert!(err.to_string().contains("no EE environment"), "{err}");
    }

    #[test]
    fn a_bare_name_present_in_two_environments_must_be_qualified() {
        let datadir = TempDatadir::new();
        seed_env(&datadir, "node");
        seed_env(&datadir, "prover");

        let db = ConsoleDb::attach(&datadir, AttachMode::ReadOnly, two_env_specs()).unwrap();
        let err = db.count("ProverTaskSchema").unwrap_err();
        assert!(
            err.to_string().contains("more than one environment"),
            "{err}"
        );
        assert!(err.to_string().contains("node/ProverTaskSchema"), "{err}");

        assert_eq!(db.count("node/ProverTaskSchema").unwrap(), 2);
        assert_eq!(db.count("prover/ProverTaskSchema").unwrap(), 2);
        assert_eq!(
            db.qualified_name("prover/ProverTaskSchema").unwrap(),
            "prover/ProverTaskSchema"
        );

        let err = db.count("da/ProverTaskSchema").unwrap_err();
        assert!(
            err.to_string().contains("unknown environment `da`"),
            "{err}"
        );
    }

    /// Edits to two environments land in two transactions, one each, and both
    /// are gone from the batch afterwards.
    #[test]
    fn a_commit_spanning_environments_lands_in_each() {
        let datadir = TempDatadir::new();
        seed_env(&datadir, "node");
        seed_env(&datadir, "prover");

        let db = ConsoleDb::attach(&datadir, AttachMode::ReadWrite, two_env_specs()).unwrap();
        db.stage_delete("prover/ProverTaskSchema", "010203")
            .unwrap();
        db.stage_delete("node/ProverTaskSchema", "040506").unwrap();
        db.stage_delete("prover/ProverTaskSchema", "040506")
            .unwrap();
        assert_eq!(
            db.staged()[0].describe(),
            "del prover/ProverTaskSchema 010203"
        );

        assert_eq!(db.commit().unwrap().total(), 3);
        assert!(db.staged().is_empty());
        assert_eq!(db.count("prover/ProverTaskSchema").unwrap(), 0);
        assert_eq!(db.count("node/ProverTaskSchema").unwrap(), 1);
        assert!(db.get("node/ProverTaskSchema", "010203").unwrap().is_some());
    }

    /// A read-write attach holds every present environment exclusively, not
    /// just the first.
    #[test]
    fn a_read_write_attach_holds_every_present_environment() {
        let datadir = TempDatadir::new();
        seed_env(&datadir, "node");
        seed_env(&datadir, "prover");

        let holder = ConsoleDb::attach(&datadir, AttachMode::ReadWrite, two_env_specs()).unwrap();
        let node = datadir.join("mdbx").join("node");
        assert!(
            MdbxEnv::open_readonly(&node, &MdbxConfig::default()).is_err(),
            "the second environment is held exclusively too"
        );
        drop(holder);
        MdbxEnv::open_readonly(&node, &MdbxConfig::default())
            .expect("released once the console detaches");
    }

    // --- Ranges ---------------------------------------------------------------

    /// Seeds finalized heights 10, 20, 30, 40 into a `node` environment whose
    /// key codec is big-endian, so cursor order is numeric order.
    fn seed_heights(datadir: &Path) {
        let node = datadir.join("mdbx").join("node");
        let env = MdbxEnv::open(
            &node,
            &MdbxConfig::default(),
            &[TableSpec::of::<ExecBlockFinalizedSchema>()],
        )
        .unwrap();
        env.update(|writer| {
            for height in [10u64, 20, 30, 40] {
                writer.put::<ExecBlockFinalizedSchema>(&height, &Hash::from([height as u8; 32]))?;
            }
            Ok::<_, DbError>(())
        })
        .unwrap();
    }

    fn heights_db(datadir: &Path) -> ConsoleDb {
        seed_heights(datadir);
        let specs = vec![EnvSpec {
            name: "node",
            tables: heights_test_tables,
        }];
        ConsoleDb::attach(datadir, AttachMode::ReadOnly, specs).unwrap()
    }

    fn between(from: &str, to: &str) -> Range {
        Range::Between {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[test]
    fn a_range_covers_its_ends_and_snaps_to_the_keys_inside_it() {
        let datadir = TempDatadir::new();
        let db = heights_db(&datadir);
        let t = "ExecBlockFinalizedSchema";

        assert_eq!(
            keys_of(&db, t, &between("20", "30"), Direction::Forward),
            vec!["20", "30"]
        );
        assert_eq!(
            keys_of(&db, t, &between("15", "35"), Direction::Forward),
            vec!["20", "30"]
        );
        assert_eq!(
            keys_of(&db, t, &between("15", "35"), Direction::Backward),
            vec!["30", "20"]
        );
        assert_eq!(
            keys_of(&db, t, &between("0", "9"), Direction::Forward),
            Vec::<String>::new()
        );
        assert_eq!(
            keys_of(&db, t, &between("41", "99"), Direction::Backward),
            Vec::<String>::new()
        );
        assert_eq!(
            keys_of(&db, t, &between("40", "99"), Direction::Backward),
            vec!["40"]
        );

        let err = db
            .scan(
                t,
                &between("30", "20"),
                Direction::Forward,
                None,
                &mut |_| Ok(true),
            )
            .unwrap_err();
        assert!(err.to_string().contains("range is empty"), "{err}");
    }

    /// A range on a table whose key bytes do not sort like its keys is refused
    /// rather than answered wrongly; the batch-id tables are borsh-encoded.
    #[test]
    fn a_range_is_refused_on_an_unordered_key() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readonly(&datadir).unwrap();

        let hash = "00".repeat(32);
        let pair = format!("{hash}:{hash}");
        let err = db
            .keys(
                "AcctProofReceiptSchema",
                &between(&pair, &pair),
                Direction::Forward,
                None,
                &mut |_| Ok(true),
            )
            .unwrap_err();
        assert!(err.to_string().contains("not stored in key order"), "{err}");

        // The raw-bytes task key is ordered, so a prefix over it works both ways.
        assert_eq!(
            keys_of(
                &db,
                "ProverTaskSchema",
                &Range::Prefix("01".into()),
                Direction::Forward
            ),
            vec!["010203"]
        );
        assert_eq!(
            keys_of(
                &db,
                "ProverTaskSchema",
                &Range::Prefix("04".into()),
                Direction::Backward
            ),
            vec!["040506"]
        );
        assert_eq!(
            keys_of(
                &db,
                "ProverTaskSchema",
                &Range::Prefix("ff".into()),
                Direction::Backward
            ),
            Vec::<String>::new()
        );
    }

    /// A prefix on a decimal key has no meaning, and says so.
    #[test]
    fn a_prefix_is_refused_on_a_decimal_key() {
        let datadir = TempDatadir::new();
        let db = heights_db(&datadir);
        let err = db
            .keys(
                "ExecBlockFinalizedSchema",
                &Range::Prefix("1".into()),
                Direction::Forward,
                None,
                &mut |_| Ok(true),
            )
            .unwrap_err();
        assert!(err.to_string().contains("no prefix form"), "{err}");
    }

    // --- Staging at scale --------------------------------------------------

    #[test]
    fn a_second_edit_to_a_staged_key_is_refused_and_names_the_first() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();

        db.stage_delete("ProverTaskSchema", "010203").unwrap();
        let err = db.stage_delete("ProverTaskSchema", "010203").unwrap_err();
        assert!(
            err.to_string().contains("already has a staged del"),
            "{err}"
        );

        let err = db
            .stage_set(
                "ProverTaskSchema",
                "010203",
                "updated_at_secs",
                FieldValue::U64(1),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("already has a staged del"),
            "{err}"
        );
        assert_eq!(db.staged().len(), 1);

        // Another key is fine, and after an abort the first key is free again.
        db.stage_delete("ProverTaskSchema", "040506").unwrap();
        db.abort();
        db.stage_delete("ProverTaskSchema", "010203").unwrap();
    }

    #[test]
    fn a_list_delete_stages_every_key_or_nothing() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();

        let both = vec!["010203".to_owned(), "040506".to_owned()];
        assert_eq!(db.stage_delete_many("ProverTaskSchema", &both).unwrap(), 2);
        assert_eq!(db.staged().len(), 2);
        db.abort();

        let with_missing = vec!["010203".to_owned(), "0a0b0c".to_owned()];
        let err = db
            .stage_delete_many("ProverTaskSchema", &with_missing)
            .unwrap_err();
        assert!(err.to_string().contains("no record at key 0a0b0c"), "{err}");
        assert!(
            db.staged().is_empty(),
            "a failed list delete staged something"
        );

        let twice = vec!["010203".to_owned(), "010203".to_owned()];
        let err = db
            .stage_delete_many("ProverTaskSchema", &twice)
            .unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
        assert!(db.staged().is_empty());
    }

    #[test]
    fn the_summary_counts_by_table_and_the_commit_reports_by_environment() {
        let datadir = TempDatadir::new();
        seed_env(&datadir, "node");
        seed_env(&datadir, "prover");
        let db = ConsoleDb::attach(&datadir, AttachMode::ReadWrite, two_env_specs()).unwrap();

        db.stage_delete("prover/ProverTaskSchema", "010203")
            .unwrap();
        db.stage_set(
            "prover/ProverTaskSchema",
            "040506",
            "updated_at_secs",
            FieldValue::U64(7),
        )
        .unwrap();
        db.stage_delete("node/ProverTaskSchema", "010203").unwrap();

        assert_eq!(
            db.staged_summary(),
            vec![
                StagedSummary {
                    env: "prover",
                    table: "ProverTaskSchema",
                    deletes: 1,
                    puts: 1
                },
                StagedSummary {
                    env: "node",
                    table: "ProverTaskSchema",
                    deletes: 1,
                    puts: 0
                },
            ]
        );

        let report = db.commit().unwrap();
        assert_eq!(report.landed, vec![("prover", 2), ("node", 1)]);
        assert_eq!(report.total(), 3);
        assert!(db.staged_summary().is_empty());
    }

    /// The first real recipe deletes thousands of rows; the path has to take
    /// that in one call and one commit.
    #[test]
    fn ten_thousand_deletes_stage_and_commit_in_one_batch() {
        let datadir = TempDatadir::new();
        let prover = datadir.join("mdbx").join("prover");
        let env = MdbxEnv::open(
            &prover,
            &MdbxConfig::small(),
            &[TableSpec::of::<ProverTaskSchema>()],
        )
        .unwrap();
        let keys: Vec<Vec<u8>> = (0..10_000u32).map(|i| i.to_be_bytes().to_vec()).collect();
        env.update(|writer| {
            for key in &keys {
                writer.put::<ProverTaskSchema>(key, &TaskRecordData::new(TaskStatus::Pending))?;
            }
            Ok::<_, DbError>(())
        })
        .unwrap();
        drop(env);

        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();
        let rendered: Vec<String> = keys.iter().map(hex::encode).collect();
        assert_eq!(
            db.stage_delete_many("ProverTaskSchema", &rendered).unwrap(),
            10_000
        );
        assert_eq!(db.staged_summary()[0].deletes, 10_000);
        assert_eq!(db.commit().unwrap().total(), 10_000);
        assert_eq!(db.count("ProverTaskSchema").unwrap(), 0);
    }

    /// The batch knows a key by its canonical spelling, so `0x` and case do
    /// not make a second edit of the same key look like a different one.
    #[test]
    fn a_key_is_one_key_to_the_batch_however_it_is_spelled() {
        let datadir = TempDatadir::new();
        seed_prover_env(&datadir);
        let db = ConsoleDb::attach_readwrite(&datadir).unwrap();

        db.stage_delete("ProverTaskSchema", "0x010203").unwrap();
        let err = db.stage_delete("ProverTaskSchema", "010203").unwrap_err();
        assert!(
            err.to_string().contains("already has a staged del"),
            "{err}"
        );
        assert_eq!(db.staged()[0].key(), "010203", "staged in canonical form");

        let err = db
            .stage_delete_many(
                "ProverTaskSchema",
                &["040506".to_owned(), "0x040506".to_owned()],
            )
            .unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }
}
