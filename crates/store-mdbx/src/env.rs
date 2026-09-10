//! The MDBX environment wrapper and typed transaction accessors.
//!
//! [`MdbxEnv`] owns a single libmdbx environment (one write-lock, many MVCC
//! readers). Callers do all work inside [`MdbxEnv::view`] / [`MdbxEnv::update`]
//! closures, which open a short-lived transaction, run the closure, and then
//! abort (read) or commit (write). This structurally enforces the store's
//! central discipline: **one logical operation = one transaction opened and
//! committed inside a single call, never held across an await or slow work.**

use std::{fs, path::Path};

use signet_libmdbx::{
    sys::PageSize,
    tx::{
        aliases::{RoTxUnsync, RwTxUnsync},
        PtrUnsync, SyncKind,
    },
    DatabaseFlags, Environment, EnvironmentFlags, Geometry, Mode, SyncMode, TransactionKind,
    TxUnsync, WriteFlags,
};

use crate::{
    codec::{BoxError, KeyCodec, Schema, ValueCodec},
    config::{MdbxConfig, MdbxSyncMode},
    error::{DbError, DbResult},
    version::{RawGet, UpgradeCtx},
};

/// Declares one table to pre-create when opening an [`MdbxEnv`].
///
/// Build these with [`TableSpec::of`] or the [`tables!`](crate::tables) macro.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    /// The sub-database name (matches [`Schema::NAME`]).
    pub name: &'static str,
}

impl TableSpec {
    /// Builds a [`TableSpec`] from a [`Schema`] type.
    pub fn of<S: Schema>() -> Self {
        Self { name: S::NAME }
    }
}

/// A single MDBX environment: the unit of the write-lock and of atomic commit.
#[derive(Debug)]
pub struct MdbxEnv {
    env: Environment,
}

impl MdbxEnv {
    /// Opens (creating if needed) an MDBX environment at `path`, pre-creating
    /// every table in `tables`.
    ///
    /// All declared tables are created in one initial write transaction so that
    /// later `view`/`update` calls can open their handles without a write.
    pub fn open(path: &Path, config: &MdbxConfig, tables: &[TableSpec]) -> DbResult<Self> {
        fs::create_dir_all(path)
            .map_err(|e| DbError::Env(format!("create dir {}: {e}", path.display())))?;

        let sync_mode = match config.sync_mode {
            MdbxSyncMode::Durable => SyncMode::Durable,
        };

        let mut builder = Environment::builder();
        builder
            .set_max_dbs(config.max_dbs)
            .set_max_readers(config.max_readers)
            .set_geometry(Geometry {
                size: Some(0..config.max_size),
                growth_step: Some(config.growth_step),
                shrink_threshold: None,
                page_size: config.page_size.map(PageSize::Set),
            })
            // Non-WRITEMAP (the builder default kind): clean ENOSPC instead of
            // SIGBUS on a full disk, and no writeable-mmap stray-pointer vector.
            .set_flags(EnvironmentFlags {
                mode: Mode::ReadWrite { sync_mode },
                ..Default::default()
            });

        let env = builder.open(path)?;

        let txn = env.begin_rw_unsync()?;
        for table in tables {
            txn.create_db(Some(table.name), DatabaseFlags::CREATE)?;
        }
        txn.commit()?;

        Ok(Self { env })
    }

    /// Runs `f` inside a read-only transaction and returns its result. The
    /// transaction is aborted when the closure returns.
    ///
    /// The closure may use any error type that a [`DbError`] converts into, so
    /// callers can return their own domain error directly.
    pub fn view<T, E>(&self, f: impl FnOnce(&Reader<'_>) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbError>,
    {
        let txn = self.env.begin_ro_unsync().map_err(DbError::from)?;
        f(&Reader { txn: &txn })
    }

    /// Runs `f` inside a read-write transaction. If `f` returns `Ok`, the
    /// transaction is committed atomically; if it returns `Err`, the
    /// transaction is aborted and no changes are persisted.
    ///
    /// The closure may use any error type that a [`DbError`] converts into, so
    /// callers can return their own domain error directly.
    pub fn update<T, E>(&self, f: impl FnOnce(&Writer<'_>) -> Result<T, E>) -> Result<T, E>
    where
        E: From<DbError>,
    {
        let txn = self.env.begin_rw_unsync().map_err(DbError::from)?;
        let out = f(&Writer { txn: &txn })?;
        txn.commit().map_err(DbError::from)?;
        Ok(out)
    }

    /// Flushes pending writes to disk. A no-op under the current
    /// [`MdbxSyncMode::Durable`] mode, which already fsyncs on every commit;
    /// kept as the flush lever a future deferred-sync mode would need.
    pub fn sync(&self, force: bool) -> DbResult<()> {
        self.env.sync(force)?;
        Ok(())
    }
}

// --- Free typed helpers, shared by `Reader` and `Writer` -----------------

// Untyped read access, so an up-converter can reach other tables through the
// ambient transaction without `UpgradeCtx` carrying the transaction's kind.
impl<K> RawGet for TxUnsync<K>
where
    K: TransactionKind + SyncKind<Access = PtrUnsync>,
{
    fn get_raw(&self, table: &'static str, key: &[u8]) -> Result<Option<Vec<u8>>, BoxError> {
        let db = self.open_db(Some(table))?;
        Ok(self.get::<Vec<u8>>(db.dbi(), key)?)
    }
}

// The read helpers are generic over the unsynchronized transaction kind so
// both `Reader` (read-only) and `Writer` (read-write) can share them; the
// `Access = PtrUnsync` bound restricts `K` to the unsynchronized `Ro`/`Rw`
// markers that `MdbxEnv` actually opens.
fn get_in<S: Schema, K>(txn: &TxUnsync<K>, key: &S::Key) -> DbResult<Option<S::Value>>
where
    K: TransactionKind + SyncKind<Access = PtrUnsync>,
{
    let db = txn.open_db(Some(S::NAME))?;
    let key_bytes = key.encode_key()?;
    match txn.get::<Vec<u8>>(db.dbi(), &key_bytes)? {
        Some(value_bytes) => Ok(Some(<S::Value as ValueCodec<S>>::decode_value(
            &value_bytes,
            &UpgradeCtx::new(txn),
        )?)),
        None => Ok(None),
    }
}

fn first_in<S: Schema, K>(txn: &TxUnsync<K>) -> DbResult<Option<(S::Key, S::Value)>>
where
    K: TransactionKind + SyncKind<Access = PtrUnsync>,
{
    let db = txn.open_db(Some(S::NAME))?;
    let mut cursor = txn.cursor(db)?;
    let ctx = UpgradeCtx::new(txn);
    cursor
        .first::<Vec<u8>, Vec<u8>>()?
        .map(|entry| decode_entry::<S>(entry, &ctx))
        .transpose()
}

fn last_in<S: Schema, K>(txn: &TxUnsync<K>) -> DbResult<Option<(S::Key, S::Value)>>
where
    K: TransactionKind + SyncKind<Access = PtrUnsync>,
{
    let db = txn.open_db(Some(S::NAME))?;
    let mut cursor = txn.cursor(db)?;
    let ctx = UpgradeCtx::new(txn);
    cursor
        .last::<Vec<u8>, Vec<u8>>()?
        .map(|entry| decode_entry::<S>(entry, &ctx))
        .transpose()
}

fn for_each_in<S: Schema, K>(
    txn: &TxUnsync<K>,
    mut f: impl FnMut(S::Key, S::Value) -> DbResult<()>,
) -> DbResult<()>
where
    K: TransactionKind + SyncKind<Access = PtrUnsync>,
{
    let db = txn.open_db(Some(S::NAME))?;
    let mut cursor = txn.cursor(db)?;
    let ctx = UpgradeCtx::new(txn);
    for entry in cursor.iter_start::<Vec<u8>, Vec<u8>>()? {
        let (key_bytes, value_bytes) = entry?;
        let key = <S::Key as KeyCodec<S>>::decode_key(&key_bytes)?;
        let value = <S::Value as ValueCodec<S>>::decode_value(&value_bytes, &ctx)?;
        f(key, value)?;
    }
    Ok(())
}

fn decode_entry<S: Schema>(
    (key_bytes, value_bytes): (Vec<u8>, Vec<u8>),
    ctx: &UpgradeCtx<'_>,
) -> DbResult<(S::Key, S::Value)> {
    Ok((
        <S::Key as KeyCodec<S>>::decode_key(&key_bytes)?,
        <S::Value as ValueCodec<S>>::decode_value(&value_bytes, ctx)?,
    ))
}

/// Read accessor handed to a [`MdbxEnv::view`] closure.
#[derive(Debug)]
pub struct Reader<'txn> {
    txn: &'txn RoTxUnsync,
}

impl<'txn> Reader<'txn> {
    /// The up-convert context for this transaction.
    ///
    /// Decoding loose bytes — a golden-fixture replay, a `db verify` pass —
    /// through the same context the normal read path uses, so their
    /// up-converters see the same snapshot.
    pub fn upgrade_ctx(&self) -> UpgradeCtx<'txn> {
        UpgradeCtx::new(self.txn)
    }

    /// Fetches the value for `key`, if present.
    pub fn get<S: Schema>(&self, key: &S::Key) -> DbResult<Option<S::Value>> {
        get_in::<S, _>(self.txn, key)
    }

    /// Returns the first (lowest-key) entry in the table, if any.
    pub fn first<S: Schema>(&self) -> DbResult<Option<(S::Key, S::Value)>> {
        first_in::<S, _>(self.txn)
    }

    /// Returns the last (highest-key) entry in the table, if any.
    pub fn last<S: Schema>(&self) -> DbResult<Option<(S::Key, S::Value)>> {
        last_in::<S, _>(self.txn)
    }

    /// Invokes `f` for every entry in the table, in ascending key order.
    pub fn for_each<S: Schema>(
        &self,
        f: impl FnMut(S::Key, S::Value) -> DbResult<()>,
    ) -> DbResult<()> {
        for_each_in::<S, _>(self.txn, f)
    }
}

/// Read-write accessor handed to a [`MdbxEnv::update`] closure.
#[derive(Debug)]
pub struct Writer<'txn> {
    txn: &'txn RwTxUnsync,
}

impl<'txn> Writer<'txn> {
    /// The up-convert context for this transaction.
    ///
    /// See [`Reader::upgrade_ctx`].
    pub fn upgrade_ctx(&self) -> UpgradeCtx<'txn> {
        UpgradeCtx::new(self.txn)
    }

    /// Fetches the value for `key`, if present.
    pub fn get<S: Schema>(&self, key: &S::Key) -> DbResult<Option<S::Value>> {
        get_in::<S, _>(self.txn, key)
    }

    /// Returns the first (lowest-key) entry in the table, if any.
    pub fn first<S: Schema>(&self) -> DbResult<Option<(S::Key, S::Value)>> {
        first_in::<S, _>(self.txn)
    }

    /// Returns the last (highest-key) entry in the table, if any.
    pub fn last<S: Schema>(&self) -> DbResult<Option<(S::Key, S::Value)>> {
        last_in::<S, _>(self.txn)
    }

    /// Invokes `f` for every entry in the table, in ascending key order.
    pub fn for_each<S: Schema>(
        &self,
        f: impl FnMut(S::Key, S::Value) -> DbResult<()>,
    ) -> DbResult<()> {
        for_each_in::<S, _>(self.txn, f)
    }

    /// Inserts or overwrites the value for `key`, in the current format.
    pub fn put<S: Schema>(&self, key: &S::Key, value: &S::Value) -> DbResult<()> {
        let db = self.txn.open_db(Some(S::NAME))?;
        let key_bytes = key.encode_key()?;
        let value_bytes = value.encode_value()?;
        self.txn
            .put(db, key_bytes, value_bytes, WriteFlags::UPSERT)?;
        Ok(())
    }

    /// Deletes `key`. Returns whether a value was removed.
    pub fn delete<S: Schema>(&self, key: &S::Key) -> DbResult<bool> {
        let db = self.txn.open_db(Some(S::NAME))?;
        let key_bytes = key.encode_key()?;
        Ok(self.txn.del(db, key_bytes, None)?)
    }

    /// Removes every entry from the table.
    pub fn clear<S: Schema>(&self) -> DbResult<()> {
        let db = self.txn.open_db(Some(S::NAME))?;
        self.txn.clear_db(db)?;
        Ok(())
    }
}

/// Behavioural tests for the environment and its typed accessors.
#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::{
        define_table, define_table_be_key, define_table_borsh, impl_be_key_codec,
        impl_raw_value_codec, impl_unit_value_codec, tables, CodecError, DbError, DbResult,
        MdbxConfig, MdbxEnv, Schema,
    };

    define_table_be_key! {
        /// Big-endian u64 key so cursor order matches numeric order.
        (Numbers) u64 => Vec<u8>
    }

    define_table_borsh! {
        /// Content-addressed blob table.
        (Blobs) [u8; 32] => u64
    }

    define_table! {
        /// A presence set: membership is the whole record.
        (Marks) u64 => ()
    }
    impl_be_key_codec!(Marks, u64);
    impl_unit_value_codec!(Marks);

    /// A raw view of the `Marks` sub-database, for inspecting the bytes a presence
    /// marker actually occupies and for planting a value it should refuse.
    struct MarksRaw;

    impl Schema for MarksRaw {
        const NAME: &'static str = "Marks";
        type Key = u64;
        type Value = Vec<u8>;
    }
    impl_be_key_codec!(MarksRaw, u64);
    impl_raw_value_codec!(MarksRaw);

    fn open() -> (tempfile::TempDir, MdbxEnv) {
        let dir = tempdir().unwrap();
        let env = MdbxEnv::open(
            dir.path(),
            &MdbxConfig::small(),
            &tables![Numbers, Blobs, Marks],
        )
        .unwrap();
        (dir, env)
    }

    #[test]
    fn put_get_roundtrip_and_overwrite() {
        let (_dir, env) = open();

        env.update(|w| w.put::<Numbers>(&7, &vec![1, 2, 3]))
            .unwrap();
        assert_eq!(
            env.view(|r| r.get::<Numbers>(&7)).unwrap(),
            Some(vec![1, 2, 3])
        );

        // upsert overwrites
        env.update(|w| w.put::<Numbers>(&7, &vec![9])).unwrap();
        assert_eq!(env.view(|r| r.get::<Numbers>(&7)).unwrap(), Some(vec![9]));

        // absent key
        assert_eq!(env.view(|r| r.get::<Numbers>(&8)).unwrap(), None);
    }

    #[test]
    fn delete_removes_key() {
        let (_dir, env) = open();
        env.update(|w| w.put::<Numbers>(&1, &vec![0])).unwrap();

        let removed = env.update(|w| w.delete::<Numbers>(&1)).unwrap();
        assert!(removed);
        assert_eq!(env.view(|r| r.get::<Numbers>(&1)).unwrap(), None);

        // deleting an absent key reports false
        assert!(!env.update(|w| w.delete::<Numbers>(&1)).unwrap());
    }

    #[test]
    fn cursor_order_is_numeric_via_big_endian_keys() {
        let (_dir, env) = open();
        env.update::<_, DbError>(|w| {
            for k in [5u64, 1, 300, 3, 256] {
                w.put::<Numbers>(&k, &vec![k as u8])?;
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(
            env.view(|r| r.first::<Numbers>()).unwrap().map(|(k, _)| k),
            Some(1)
        );
        assert_eq!(
            env.view(|r| r.last::<Numbers>()).unwrap().map(|(k, _)| k),
            Some(300)
        );

        let mut seen = Vec::new();
        env.view(|r| {
            r.for_each::<Numbers>(|k, _| {
                seen.push(k);
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(seen, vec![1, 3, 5, 256, 300]);
    }

    #[test]
    fn update_commits_all_tables_atomically() {
        let (_dir, env) = open();
        env.update::<_, DbError>(|w| {
            w.put::<Numbers>(&42, &vec![42])?;
            w.put::<Blobs>(&[7u8; 32], &99)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(env.view(|r| r.get::<Numbers>(&42)).unwrap(), Some(vec![42]));
        assert_eq!(env.view(|r| r.get::<Blobs>(&[7u8; 32])).unwrap(), Some(99));
    }

    #[test]
    fn update_error_aborts_the_whole_transaction() {
        let (_dir, env) = open();
        env.update(|w| w.put::<Numbers>(&1, &vec![1])).unwrap();

        // A closure that writes then fails must leave no trace of its writes.
        let res: DbResult<()> = env.update(|w| {
            w.put::<Numbers>(&2, &vec![2])?;
            w.put::<Blobs>(&[1u8; 32], &7)?;
            Err(DbError::Env("boom".into()))
        });
        assert!(res.is_err());

        assert_eq!(env.view(|r| r.get::<Numbers>(&1)).unwrap(), Some(vec![1]));
        assert_eq!(env.view(|r| r.get::<Numbers>(&2)).unwrap(), None);
        assert_eq!(env.view(|r| r.get::<Blobs>(&[1u8; 32])).unwrap(), None);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = tempdir().unwrap();
        {
            let env =
                MdbxEnv::open(dir.path(), &MdbxConfig::small(), &tables![Numbers, Blobs]).unwrap();
            env.update(|w| w.put::<Numbers>(&11, &vec![1, 1])).unwrap();
        }
        // reopen the same directory
        let env =
            MdbxEnv::open(dir.path(), &MdbxConfig::small(), &tables![Numbers, Blobs]).unwrap();
        assert_eq!(
            env.view(|r| r.get::<Numbers>(&11)).unwrap(),
            Some(vec![1, 1])
        );
    }

    #[test]
    fn a_presence_marker_stores_the_key_and_no_value_bytes() {
        let (_dir, env) = open();
        env.update(|w| w.put::<Marks>(&7, &())).unwrap();

        assert_eq!(env.view(|r| r.get::<Marks>(&7)).unwrap(), Some(()));
        assert_eq!(env.view(|r| r.get::<Marks>(&8)).unwrap(), None);
        assert_eq!(
            env.view(|r| r.get::<MarksRaw>(&7)).unwrap(),
            Some(Vec::new()),
            "membership must cost no value bytes"
        );
    }

    #[test]
    fn a_presence_marker_refuses_a_value_it_did_not_write() {
        let (_dir, env) = open();
        env.update(|w| w.put::<MarksRaw>(&7, &vec![1])).unwrap();

        let err = env.view(|r| r.get::<Marks>(&7)).unwrap_err();
        assert!(
            matches!(err, DbError::Codec(CodecError::Decode { .. })),
            "expected a decode refusal, got {err:?}"
        );
    }
}
