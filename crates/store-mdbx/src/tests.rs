//! Behavioural tests for the MDBX table toolkit.

use tempfile::tempdir;

use crate::{
    define_table_be_key, define_table_borsh, tables, DbError, DbResult, MdbxConfig, MdbxEnv,
};

define_table_be_key! {
    /// Big-endian u64 key so cursor order matches numeric order.
    (Numbers) u64 => Vec<u8>
}

define_table_borsh! {
    /// Content-addressed blob table.
    (Blobs) [u8; 32] => u64
}

fn open() -> (tempfile::TempDir, MdbxEnv) {
    let dir = tempdir().unwrap();
    let env = MdbxEnv::open(dir.path(), &MdbxConfig::small(), &tables![Numbers, Blobs]).unwrap();
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
    let env = MdbxEnv::open(dir.path(), &MdbxConfig::small(), &tables![Numbers, Blobs]).unwrap();
    assert_eq!(
        env.view(|r| r.get::<Numbers>(&11)).unwrap(),
        Some(vec![1, 1])
    );
}
