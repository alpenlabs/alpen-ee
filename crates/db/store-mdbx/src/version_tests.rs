//! Behavioural tests for per-value schema versioning.
//!
//! The store's guarantee is that a binary decodes every format it has ever
//! written, refuses anything newer without misreading it, and never blocks
//! startup to do so. These exercise that from the outside: old bytes are placed
//! on disk through a raw view of the same sub-database, then read back through
//! the normal typed accessor.

use borsh::{BorshDeserialize, BorshSerialize};
use tempfile::tempdir;

use crate::{
    define_table_borsh, impl_borsh_key_codec, impl_raw_value_codec, impl_schema_version_borsh,
    tables,
    version::fixtures::{check_fixtures, FixtureError, GoldenFixture},
    versioned_value, CodecError, DbError, MdbxConfig, MdbxEnv, Regime, Schema, UpConvert,
    UpgradeCtx, VersionedValue, MAX_UPGRADE_DEPTH,
};

type Hash = [u8; 32];

// --- A three-version value, the last converter reading another table ----

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub(crate) struct AccountV1 {
    balance: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub(crate) struct AccountV2 {
    balance: u64,
    nonce: u64,
    owner: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub(crate) struct AccountV3 {
    balance: u64,
    nonce: u64,
    owner: Hash,
    code_hash: Hash,
}

impl_schema_version_borsh!(Account, AccountV1, 1);
impl_schema_version_borsh!(Account, AccountV2, 2);
impl_schema_version_borsh!(Account, AccountV3, 3);

impl UpConvert<AccountV2> for AccountV1 {
    fn up_convert(self, _ctx: &UpgradeCtx<'_>) -> Result<AccountV2, CodecError> {
        Ok(AccountV2 {
            balance: self.balance,
            nonce: 0,
            owner: [0; 32],
        })
    }
}

impl UpConvert<AccountV3> for AccountV2 {
    /// Derives the new field from another table, through the same
    /// version-dispatching accessor.
    fn up_convert(self, ctx: &UpgradeCtx<'_>) -> Result<AccountV3, CodecError> {
        let code_hash = ctx.get::<Codes>(&self.owner)?.unwrap_or([0; 32]);
        Ok(AccountV3 {
            balance: self.balance,
            nonce: self.nonce,
            owner: self.owner,
            code_hash,
        })
    }
}

versioned_value! {
    /// Account state, currently at v3.
    pub(crate) Account {
        1 => AccountV1,
        2 => AccountV2,
        3 => AccountV3,
    }
}

crate::define_table!(
    /// Accounts, version-dispatched on read and written at the current version.
    (Accounts) Hash => Account
);
impl_borsh_key_codec!(Accounts, Hash);
crate::impl_versioned_value_codec!(Accounts, Account);

define_table_borsh! {
    /// Owner to code hash, read by the v2 -> v3 up-converter.
    (Codes) Hash => Hash
}

/// A raw view of the `Accounts` sub-database, for planting bytes an older
/// binary would have written and for inspecting the tag actually on disk.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AccountsRaw;

impl Schema for AccountsRaw {
    const NAME: &'static str = "Accounts";
    type Key = Hash;
    type Value = Vec<u8>;
}
impl_borsh_key_codec!(AccountsRaw, Hash);
impl_raw_value_codec!(AccountsRaw);

// --- An immutable table ---------------------------------------------------

define_table_borsh! {
    /// Records that are fixed once written: existing values are frozen.
    (Records, immutable) u64 => Vec<u8>
}

// --- A value whose up-converter reads its own table (a forbidden cycle) ---

#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub(crate) struct LoopyV1 {
    key: Hash,
}

#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub(crate) struct LoopyV2 {
    key: Hash,
}

impl_schema_version_borsh!(Loopy, LoopyV1, 1);
impl_schema_version_borsh!(Loopy, LoopyV2, 2);

impl UpConvert<LoopyV2> for LoopyV1 {
    fn up_convert(self, ctx: &UpgradeCtx<'_>) -> Result<LoopyV2, CodecError> {
        ctx.get::<Loopies>(&self.key)?;
        Ok(LoopyV2 { key: self.key })
    }
}

versioned_value! {
    /// A value whose up-converter reads the table it lives in.
    pub(crate) Loopy {
        1 => LoopyV1,
        2 => LoopyV2,
    }
}

crate::define_table!(
    /// Table backing the cycle test.
    (Loopies) Hash => Loopy
);
impl_borsh_key_codec!(Loopies, Hash);
crate::impl_versioned_value_codec!(Loopies, Loopy);

// --- Helpers --------------------------------------------------------------

fn open() -> (tempfile::TempDir, MdbxEnv) {
    let dir = tempdir().unwrap();
    let env = MdbxEnv::open(
        dir.path(),
        &MdbxConfig::small(),
        &tables![Accounts, Codes, Records, Loopies],
    )
    .unwrap();
    (dir, env)
}

/// Encodes a payload the way a binary shipping only that version would have.
fn tagged(version: u8, payload: &impl BorshSerialize) -> Vec<u8> {
    let mut bytes = vec![version];
    bytes.extend_from_slice(&borsh::to_vec(payload).unwrap());
    bytes
}

const KEY: Hash = [1; 32];
const OWNER: Hash = [0xaa; 32];
const CODE: Hash = [0xbb; 32];

// --- Version dispatch on read --------------------------------------------

#[test]
fn old_value_decodes_and_folds_up_to_current() {
    let (_dir, env) = open();

    // What a v1-era binary wrote.
    let old = tagged(1, &AccountV1 { balance: 7 });
    env.update(|w| w.put::<AccountsRaw>(&KEY, &old)).unwrap();

    let account = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
    assert_eq!(
        account,
        AccountV3 {
            balance: 7,
            nonce: 0,
            owner: [0; 32],
            code_hash: [0; 32],
        }
    );
}

#[test]
fn up_converter_derives_a_field_from_another_table() {
    let (_dir, env) = open();

    env.update::<_, DbError>(|w| {
        w.put::<Codes>(&OWNER, &CODE)?;
        w.put::<AccountsRaw>(
            &KEY,
            &tagged(
                2,
                &AccountV2 {
                    balance: 9,
                    nonce: 3,
                    owner: OWNER,
                },
            ),
        )
    })
    .unwrap();

    let account = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
    assert_eq!(account.code_hash, CODE);
    assert_eq!(account.nonce, 3);
}

#[test]
fn read_does_not_write_back_and_cold_keys_stay_old() {
    let (_dir, env) = open();
    let cold: Hash = [2; 32];

    env.update::<_, DbError>(|w| {
        w.put::<AccountsRaw>(&KEY, &tagged(1, &AccountV1 { balance: 1 }))?;
        w.put::<AccountsRaw>(&cold, &tagged(1, &AccountV1 { balance: 2 }))
    })
    .unwrap();

    // Reading both up-converts in memory only.
    env.view::<_, DbError>(|r| {
        r.get::<Accounts>(&KEY)?;
        r.get::<Accounts>(&cold)?;
        Ok(())
    })
    .unwrap();

    assert_eq!(
        env.view(|r| r.get::<AccountsRaw>(&KEY)).unwrap().unwrap()[0],
        1,
        "a read must not rewrite the value"
    );

    // A natural write drifts that one key forward; the cold key stays at v1.
    let touched = env.view(|r| r.get::<Accounts>(&KEY)).unwrap().unwrap();
    env.update(|w| w.put::<Accounts>(&KEY, &touched)).unwrap();

    assert_eq!(
        env.view(|r| r.get::<AccountsRaw>(&KEY)).unwrap().unwrap()[0],
        3,
        "a write lands in the current format"
    );
    assert_eq!(
        env.view(|r| r.get::<AccountsRaw>(&cold)).unwrap().unwrap()[0],
        1,
        "an untouched key keeps its old format"
    );
}

#[test]
fn scans_dispatch_per_value_across_mixed_versions() {
    let (_dir, env) = open();

    env.update::<_, DbError>(|w| {
        w.put::<Codes>(&OWNER, &CODE)?;
        w.put::<AccountsRaw>(&[1; 32], &tagged(1, &AccountV1 { balance: 1 }))?;
        w.put::<AccountsRaw>(
            &[2; 32],
            &tagged(
                2,
                &AccountV2 {
                    balance: 2,
                    nonce: 0,
                    owner: OWNER,
                },
            ),
        )?;
        w.put::<Accounts>(
            &[3; 32],
            &AccountV3 {
                balance: 3,
                nonce: 0,
                owner: [0; 32],
                code_hash: [0; 32],
            },
        )
    })
    .unwrap();

    let mut balances = Vec::new();
    env.view(|r| {
        r.for_each::<Accounts>(|_, v| {
            balances.push(v.balance);
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(balances, vec![1, 2, 3]);
}

// --- Refusals: loud, never silent ----------------------------------------

#[test]
fn a_newer_version_is_refused_with_a_typed_error() {
    let (_dir, env) = open();

    // What a future binary would have written.
    env.update(|w| w.put::<AccountsRaw>(&KEY, &vec![9, 0, 0, 0]))
        .unwrap();

    let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::Codec(CodecError::NewerVersion {
                tag: 9,
                current: 3,
                ..
            })
        ),
        "expected a newer-version refusal, got {err:?}"
    );
}

#[test]
fn an_empty_value_reports_a_missing_tag() {
    let (_dir, env) = open();
    env.update(|w| w.put::<AccountsRaw>(&KEY, &Vec::new()))
        .unwrap();

    let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
    assert!(
        matches!(err, DbError::Codec(CodecError::MissingVersionTag { .. })),
        "expected a missing-tag error, got {err:?}"
    );
}

#[test]
fn a_known_tag_with_wrong_bytes_fails_rather_than_misreading() {
    let (_dir, env) = open();
    // Tagged v2, but only a v1-sized payload behind it.
    let mut bytes = vec![2];
    bytes.extend_from_slice(&borsh::to_vec(&AccountV1 { balance: 5 }).unwrap());
    env.update(|w| w.put::<AccountsRaw>(&KEY, &bytes)).unwrap();

    let err = env.view(|r| r.get::<Accounts>(&KEY)).unwrap_err();
    assert!(
        matches!(err, DbError::Codec(CodecError::Decode { .. })),
        "expected a decode error, got {err:?}"
    );
}

#[test]
fn a_detached_context_refuses_a_table_read() {
    let ctx = UpgradeCtx::detached();
    let bytes = tagged(
        2,
        &AccountV2 {
            balance: 1,
            nonce: 1,
            owner: OWNER,
        },
    );

    let err = Account::decode_tagged(&bytes, &ctx).unwrap_err();
    assert!(
        matches!(err, CodecError::NoUpgradeContext { .. }),
        "expected a no-context error, got {err:?}"
    );

    // A converter that only defaults still works without a transaction.
    let v1 = tagged(1, &AccountV1 { balance: 4 });
    let err = Account::decode_tagged(&v1, &ctx).unwrap_err();
    assert!(
        matches!(err, CodecError::NoUpgradeContext { .. }),
        "the v1 value folds through v2 -> v3, which does read a table: {err:?}"
    );
}

#[test]
fn an_up_converter_read_cycle_is_refused() {
    let (_dir, env) = open();

    // A v1 value whose up-converter reads the very key being decoded: each
    // decode nests one level deeper instead of terminating.
    env.update(|w| w.put::<LoopiesRaw>(&KEY, &tagged(1, &LoopyV1 { key: KEY })))
        .unwrap();

    let err = env.view(|r| r.get::<Loopies>(&KEY)).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::Codec(CodecError::UpgradeContextDepth { depth, .. })
                if depth == MAX_UPGRADE_DEPTH
        ),
        "expected a depth refusal, got {err:?}"
    );
}

/// A raw view of the `Loopies` sub-database.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LoopiesRaw;

impl Schema for LoopiesRaw {
    const NAME: &'static str = "Loopies";
    type Key = Hash;
    type Value = Vec<u8>;
}
impl_borsh_key_codec!(LoopiesRaw, Hash);
impl_raw_value_codec!(LoopiesRaw);

// --- Regimes --------------------------------------------------------------

#[test]
fn regimes_are_declared_on_the_table() {
    assert_eq!(<Records as Schema>::REGIME, Regime::Immutable);
    assert_eq!(<Accounts as Schema>::REGIME, Regime::Mutable);
}

#[test]
fn an_immutable_table_refuses_to_rewrite_an_existing_value() {
    let (_dir, env) = open();
    env.update(|w| w.put::<Records>(&1, &vec![1, 2, 3]))
        .unwrap();

    // Re-putting the same content is idempotent, not an error.
    env.update(|w| w.put::<Records>(&1, &vec![1, 2, 3]))
        .unwrap();

    let err = env.update(|w| w.put::<Records>(&1, &vec![9])).unwrap_err();
    assert!(
        matches!(err, DbError::ImmutableOverwrite { .. }),
        "expected an immutable-overwrite refusal, got {err:?}"
    );
    assert_eq!(
        env.view(|r| r.get::<Records>(&1)).unwrap(),
        Some(vec![1, 2, 3]),
        "the refused write must leave the value untouched"
    );
}

#[test]
fn an_immutable_table_still_allows_insert_and_delete() {
    let (_dir, env) = open();
    env.update(|w| w.put::<Records>(&1, &vec![1])).unwrap();
    assert!(env.update(|w| w.delete::<Records>(&1)).unwrap());
    // Deleted, so this is an insert rather than a rewrite.
    env.update(|w| w.put::<Records>(&1, &vec![2])).unwrap();
    assert_eq!(env.view(|r| r.get::<Records>(&1)).unwrap(), Some(vec![2]));
}

// --- Golden fixtures ------------------------------------------------------

#[test]
fn golden_fixtures_replay_every_shipped_version() {
    let (_dir, env) = open();
    env.update(|w| w.put::<Codes>(&OWNER, &CODE)).unwrap();

    let v1 = tagged(1, &AccountV1 { balance: 1 });
    let v2 = tagged(
        2,
        &AccountV2 {
            balance: 2,
            nonce: 2,
            owner: OWNER,
        },
    );
    let v3 = tagged(
        3,
        &AccountV3 {
            balance: 3,
            nonce: 3,
            owner: OWNER,
            code_hash: CODE,
        },
    );
    let fixtures = [
        GoldenFixture::new(1, &v1),
        GoldenFixture::new(2, &v2),
        GoldenFixture::new(3, &v3),
    ];

    let decoded: Vec<Account> = env
        .view::<_, DbError>(|r| Ok(check_fixtures::<Account>(&fixtures, &r.upgrade_ctx()).unwrap()))
        .unwrap();

    assert_eq!(decoded.len(), 3);
    assert_eq!(decoded[0].balance, 1);
    assert_eq!(decoded[1].code_hash, CODE, "v2 folds through its converter");
    assert_eq!(decoded[2].nonce, 3);
}

#[test]
fn a_missing_fixture_fails_the_check() {
    let v1 = tagged(1, &AccountV1 { balance: 1 });
    let err = check_fixtures::<Account>(&[GoldenFixture::new(1, &v1)], &UpgradeCtx::detached())
        .unwrap_err();

    assert!(
        matches!(err, FixtureError::MissingVersion { version: 2, .. }),
        "expected a missing-version report, got {err:?}"
    );
}

#[test]
fn a_mislabelled_fixture_fails_the_check() {
    let v1 = tagged(1, &AccountV1 { balance: 1 });
    let v2 = tagged(
        2,
        &AccountV2 {
            balance: 2,
            nonce: 2,
            owner: OWNER,
        },
    );
    // The third fixture claims v3 but carries v1 bytes.
    let fixtures = [
        GoldenFixture::new(1, &v1),
        GoldenFixture::new(2, &v2),
        GoldenFixture::new(3, &v1),
    ];

    let err = check_fixtures::<Account>(&fixtures, &UpgradeCtx::detached()).unwrap_err();
    assert!(
        matches!(
            err,
            FixtureError::TagMismatch {
                version: 3,
                tag: 1,
                ..
            }
        ),
        "expected a tag mismatch, got {err:?}"
    );
}

// --- Declared metadata ----------------------------------------------------

#[test]
fn the_family_reports_its_version_chain() {
    assert_eq!(Account::FAMILY, "Account");
    assert_eq!(Account::CURRENT_VERSION, 3);
    assert_eq!(Account::VERSIONS, &[1, 2, 3]);
}

#[test]
fn a_round_trip_through_the_store_is_stable() {
    let (_dir, env) = open();
    let account = AccountV3 {
        balance: 42,
        nonce: 7,
        owner: OWNER,
        code_hash: CODE,
    };
    env.update(|w| w.put::<Accounts>(&KEY, &account)).unwrap();
    assert_eq!(
        env.view(|r| r.get::<Accounts>(&KEY)).unwrap(),
        Some(account)
    );
}
