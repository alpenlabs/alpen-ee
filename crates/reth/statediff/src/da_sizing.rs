//! Worst-case DA size of a state diff, computed without encoding it.
//!
//! Anything that can describe itself as a stream of [`DaEntry`] values implements
//! [`DaSizable`]. [`estimate_da_size`] then sums a fixed upper-bound byte cost per
//! entry, so the result never falls short of the bytes a
//! [`BatchStateDiff`](crate::batch::BatchStateDiff) actually encodes to while staying cheap enough
//! to run on every sealing decision.
//!
//! The constants mirror the `Codec` impls in [`crate::batch`]. Change them together.

use revm_primitives::KECCAK_EMPTY;

use crate::{
    batch::BatchBuilder,
    block::{AccountSnapshot, BlockStateChanges},
};

/// Fixed 20-byte address key of an account or storage entry (`CodecAddress`).
const ACCOUNT_KEY_BYTES: u64 = 20;

/// 1-byte `AccountChange` discriminant on every account entry.
const ACCOUNT_CHANGE_TAG_BYTES: u64 = 1;

/// `AccountDiff` compound header (1) + signed balance delta (34) + signed nonce varint (10).
/// The code-hash register is charged separately via [`CODE_HASH_BYTES`].
const ACCOUNT_INFO_BYTES: u64 = 1 + 34 + 10;

/// Fixed 32-byte code-hash register (`CodecB256`), present only when the code changed.
const CODE_HASH_BYTES: u64 = 32;

/// Fixed 32-byte big-endian storage slot key.
const SLOT_KEY_BYTES: u64 = 32;

/// `TrimmedStorageValue`: 1 length byte + up to 32 value bytes.
const SLOT_VALUE_BYTES: u64 = 1 + 32;

/// Fixed 4-byte `u32` length prefix on every map and on `CodecBytes`.
const LEN_PREFIX_BYTES: u64 = 4;

/// Fixed 32-byte code-hash key of a `deployed_bytecodes` entry (`CodecB256`).
const BYTECODE_KEY_BYTES: u64 = 32;

/// One unit of a state diff that reaches DA.
///
/// Entries carry only what sizing needs, so the same estimator serves any source that
/// can classify its changes this way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaEntry {
    /// One changed account.
    Account {
        /// The account no longer exists after the diff.
        deleted: bool,
        /// The code-hash register is written.
        code_changed: bool,
    },
    /// One account with at least one changed storage slot.
    Storage {
        /// Number of changed slots. Never zero.
        changed_slots: u64,
    },
    /// One deployed bytecode.
    Bytecode {
        /// Raw bytecode length in bytes.
        len: u64,
    },
}

impl DaEntry {
    /// Classifies an account transition, or `None` when nothing changed.
    ///
    /// Same rule as `AccountDiff::from_account_snapshot`: the code-hash register is written
    /// when the hash differs from the original, or on creation with non-empty code.
    pub fn for_account(
        original: Option<&AccountSnapshot>,
        current: Option<&AccountSnapshot>,
    ) -> Option<Self> {
        let (deleted, code_changed) = match (original, current) {
            (None, None) => return None,
            (Some(original), Some(current)) if original == current => return None,
            (_, None) => (true, false),
            (None, Some(current)) => (false, current.code_hash != KECCAK_EMPTY),
            (Some(original), Some(current)) => (false, original.code_hash != current.code_hash),
        };
        Some(Self::Account {
            deleted,
            code_changed,
        })
    }

    /// Wraps a changed-slot count, or `None` when no slot changed.
    pub fn for_storage(changed_slots: u64) -> Option<Self> {
        (changed_slots > 0).then_some(Self::Storage { changed_slots })
    }

    /// Worst-case encoded size of this entry in bytes.
    pub fn da_size(self) -> u64 {
        match self {
            Self::Account { deleted: true, .. } => ACCOUNT_KEY_BYTES + ACCOUNT_CHANGE_TAG_BYTES,
            Self::Account {
                deleted: false,
                code_changed,
            } => {
                let base = ACCOUNT_KEY_BYTES + ACCOUNT_CHANGE_TAG_BYTES + ACCOUNT_INFO_BYTES;
                if code_changed {
                    base + CODE_HASH_BYTES
                } else {
                    base
                }
            }
            Self::Storage { changed_slots } => (ACCOUNT_KEY_BYTES + LEN_PREFIX_BYTES)
                .saturating_add(changed_slots.saturating_mul(SLOT_KEY_BYTES + SLOT_VALUE_BYTES)),
            Self::Bytecode { len } => (BYTECODE_KEY_BYTES + LEN_PREFIX_BYTES).saturating_add(len),
        }
    }
}

/// A state diff that can enumerate its DA-relevant entries.
///
/// Implementors decide what counts as changed. Entries that would not be encoded, such
/// as values that reverted within a batch, must not be yielded.
pub trait DaSizable {
    /// Yields every entry that contributes to the encoded size.
    fn da_entries(&self) -> impl Iterator<Item = DaEntry> + '_;
}

impl DaSizable for BatchBuilder {
    fn da_entries(&self) -> impl Iterator<Item = DaEntry> + '_ {
        let accounts = self.accounts.values().filter_map(|tracked| {
            DaEntry::for_account(tracked.original.as_ref(), tracked.current.as_ref())
        });

        let storage = self.storage.values().filter_map(|slots| {
            DaEntry::for_storage(slots.values().filter(|s| !s.is_unchanged()).count() as u64)
        });

        let bytecodes = self
            .deployed_bytecodes
            .values()
            .map(|code| DaEntry::Bytecode {
                len: code.len() as u64,
            });

        accounts.chain(storage).chain(bytecodes)
    }
}

impl DaSizable for BlockStateChanges {
    fn da_entries(&self) -> impl Iterator<Item = DaEntry> + '_ {
        let accounts = self.accounts.values().filter_map(|change| {
            DaEntry::for_account(change.original.as_ref(), change.current.as_ref())
        });

        let storage = self.storage.values().filter_map(|diff| {
            DaEntry::for_storage(
                diff.slots
                    .values()
                    .filter(|(original, current)| original != current)
                    .count() as u64,
            )
        });

        let bytecodes = self
            .deployed_bytecodes
            .values()
            .map(|code| DaEntry::Bytecode {
                len: code.len() as u64,
            });

        accounts.chain(storage).chain(bytecodes)
    }
}

/// Worst-case encoded size, in bytes, of the [`BatchStateDiff`](crate::batch::BatchStateDiff)
/// `state` describes.
pub fn estimate_da_size(state: &impl DaSizable) -> u64 {
    // Length prefixes of the three top-level maps.
    let framing = 3 * LEN_PREFIX_BYTES;
    state
        .da_entries()
        .fold(framing, |size, entry| size.saturating_add(entry.da_size()))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use strata_codec::encode_to_vec;

    use super::*;
    use crate::{
        block::{AccountSnapshot, BlockAccountChange, BlockStateChanges},
        test_utils::{
            account_change, addr, block_diff, bytecode, deployed_bytecode, hash, slot, snapshot,
            storage_change, value,
        },
    };

    fn builder_for(blocks: &[BlockStateChanges]) -> BatchBuilder {
        let mut builder = BatchBuilder::new();
        for block in blocks {
            builder.apply_block(block);
        }
        builder
    }

    /// Encoded length of the real batch diff for `blocks`.
    fn encoded_len(blocks: &[BlockStateChanges]) -> u64 {
        encode_to_vec(&builder_for(blocks).build()).unwrap().len() as u64
    }

    #[test]
    fn empty_matches_encoding() {
        assert_eq!(estimate_da_size(&BatchBuilder::new()), encoded_len(&[]));
    }

    #[test]
    fn bounds_encoded_size() {
        let created = addr(0x11);
        let updated = addr(0x22);
        let deleted = addr(0x33);
        let code_hash = hash(0xaa);

        let mut block = block_diff();
        block.accounts.insert(
            created,
            BlockAccountChange {
                original: None,
                current: Some(AccountSnapshot {
                    balance: U256::MAX,
                    nonce: 1,
                    code_hash,
                }),
            },
        );
        account_change(
            &mut block,
            updated,
            Some(snapshot(0, 0, KECCAK_EMPTY)),
            Some(snapshot(u64::MAX, u64::MAX, KECCAK_EMPTY)),
        );
        account_change(
            &mut block,
            deleted,
            Some(snapshot(5, 5, KECCAK_EMPTY)),
            None,
        );
        storage_change(&mut block, created, slot(1), U256::ZERO, U256::MAX);
        storage_change(&mut block, created, slot(2), value(7), U256::ZERO);
        deployed_bytecode(&mut block, code_hash, bytecode(&[0x60u8; 300]));

        let blocks = [block];
        let actual = encoded_len(&blocks);
        let estimate = estimate_da_size(&builder_for(&blocks));
        assert!(
            estimate >= actual,
            "estimate {estimate} underestimates encoded size {actual}"
        );
        // Slack is bounded by the worst-case headroom of two live accounts and two slots.
        assert!(estimate - actual < 2 * ACCOUNT_INFO_BYTES + 2 * SLOT_VALUE_BYTES);
    }

    #[test]
    fn reverted_entries_cost_nothing() {
        let address = addr(0x11);

        let mut block_one = block_diff();
        account_change(
            &mut block_one,
            address,
            Some(snapshot(0, 0, KECCAK_EMPTY)),
            Some(snapshot(1000, 0, KECCAK_EMPTY)),
        );
        storage_change(&mut block_one, address, slot(1), U256::ZERO, value(100));

        let mut block_two = block_diff();
        account_change(
            &mut block_two,
            address,
            Some(snapshot(1000, 0, KECCAK_EMPTY)),
            Some(snapshot(0, 0, KECCAK_EMPTY)),
        );
        storage_change(&mut block_two, address, slot(1), value(100), U256::ZERO);

        let builder = builder_for(&[block_one, block_two]);
        assert_eq!(builder.da_entries().count(), 0);
        assert_eq!(
            estimate_da_size(&builder),
            estimate_da_size(&BatchBuilder::new())
        );
    }

    #[test]
    fn code_hash_charged_only_on_code_change() {
        let address = addr(0x11);
        let contract = hash(0x11);

        let mut called = block_diff();
        account_change(
            &mut called,
            address,
            Some(snapshot(0, 0, contract)),
            Some(snapshot(1, 0, contract)),
        );
        let mut delegated = block_diff();
        account_change(
            &mut delegated,
            address,
            Some(snapshot(0, 0, KECCAK_EMPTY)),
            Some(snapshot(1, 0, contract)),
        );

        let called_entries: Vec<_> = builder_for(&[called]).da_entries().collect();
        let delegated_entries: Vec<_> = builder_for(&[delegated]).da_entries().collect();
        assert_eq!(
            called_entries,
            [DaEntry::Account {
                deleted: false,
                code_changed: false
            }]
        );
        assert_eq!(
            delegated_entries,
            [DaEntry::Account {
                deleted: false,
                code_changed: true
            }]
        );
        assert_eq!(
            delegated_entries[0].da_size(),
            called_entries[0].da_size() + CODE_HASH_BYTES
        );
    }

    #[test]
    fn block_entries_match_builder_of_that_block() {
        let address = addr(0x11);
        let code_hash = hash(0xaa);

        let mut block = block_diff();
        account_change(&mut block, address, None, Some(snapshot(1, 1, code_hash)));
        account_change(
            &mut block,
            addr(0x22),
            Some(snapshot(5, 5, KECCAK_EMPTY)),
            None,
        );
        storage_change(&mut block, address, slot(1), U256::ZERO, value(9));
        // A slot recorded with equal values encodes nothing and must not be sized.
        storage_change(&mut block, address, slot(2), value(3), value(3));
        deployed_bytecode(&mut block, code_hash, bytecode(&[0x60u8; 40]));

        let builder = builder_for(&[block.clone()]);
        let block_entries: Vec<_> = block.da_entries().collect();
        let builder_entries: Vec<_> = builder.da_entries().collect();
        assert_eq!(block_entries, builder_entries);
        assert_eq!(estimate_da_size(&block), estimate_da_size(&builder));
    }

    #[test]
    fn created_with_empty_code_does_not_charge_code_hash() {
        let mut block = block_diff();
        account_change(
            &mut block,
            addr(0x11),
            None,
            Some(snapshot(1, 1, KECCAK_EMPTY)),
        );

        let entries: Vec<_> = builder_for(&[block]).da_entries().collect();
        assert_eq!(
            entries,
            [DaEntry::Account {
                deleted: false,
                code_changed: false
            }]
        );
    }

    #[test]
    fn deleted_account_carries_only_its_tag() {
        let mut block = block_diff();
        account_change(
            &mut block,
            addr(0x11),
            Some(snapshot(5, 5, KECCAK_EMPTY)),
            None,
        );

        let entries: Vec<_> = builder_for(&[block]).da_entries().collect();
        assert_eq!(
            entries,
            [DaEntry::Account {
                deleted: true,
                code_changed: false
            }]
        );
        assert_eq!(
            entries[0].da_size(),
            ACCOUNT_KEY_BYTES + ACCOUNT_CHANGE_TAG_BYTES
        );
    }
}
