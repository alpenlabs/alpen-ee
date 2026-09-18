//! Read-only view of a batch builder with one more block applied.
//!
//! [`BatchWithBlock`] yields the DA entries a [`BatchBuilder`] would hold after
//! `apply_block`, without cloning or mutating it. The sealing policy sizes this view to
//! decide whether a block still fits, then applies the block for real only if it does.

use alpen_reth_statediff::{BatchBuilder, BlockStateChanges, DaEntry, DaSizable};

/// `builder` as it would look after applying `block`.
///
/// Merges with the same rule as `apply_block`: the builder's original wins where it
/// already tracks an entry, the block's current wins where the block touches one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BatchWithBlock<'a> {
    builder: &'a BatchBuilder,
    block: &'a BlockStateChanges,
}

impl<'a> BatchWithBlock<'a> {
    pub(crate) fn new(builder: &'a BatchBuilder, block: &'a BlockStateChanges) -> Self {
        Self { builder, block }
    }
}

impl DaSizable for BatchWithBlock<'_> {
    fn da_entries(&self) -> impl Iterator<Item = DaEntry> + '_ {
        let tracked_accounts = self.builder.accounts();
        let block_accounts = &self.block.accounts;

        // Accounts the builder already tracks, with `current` overridden where the block
        // touches them.
        let merged_accounts = tracked_accounts.iter().filter_map(|(addr, tracked)| {
            let current = match block_accounts.get(addr) {
                Some(change) => change.current.as_ref(),
                None => tracked.current().as_ref(),
            };
            DaEntry::for_account(tracked.original().as_ref(), current)
        });
        // Accounts the block introduces.
        let new_accounts = block_accounts
            .iter()
            .filter(|(addr, _)| !tracked_accounts.contains_key(*addr))
            .filter_map(|(_, change)| {
                DaEntry::for_account(change.original.as_ref(), change.current.as_ref())
            });

        let tracked_storage = self.builder.storage();
        let block_storage = &self.block.storage;

        let merged_storage = tracked_storage.iter().filter_map(|(addr, slots)| {
            let block_slots = block_storage.get(addr).map(|diff| &diff.slots);

            let tracked_changed = slots
                .iter()
                .filter(|(key, tracked)| {
                    let current = block_slots
                        .and_then(|block_slots| block_slots.get(key))
                        .map(|(_, current)| current)
                        .unwrap_or(tracked.current());
                    tracked.original() != current
                })
                .count();
            let new_changed = block_slots.map_or(0, |block_slots| {
                block_slots
                    .iter()
                    .filter(|(key, (original, current))| {
                        !slots.contains_key(key) && original != current
                    })
                    .count()
            });

            DaEntry::for_storage((tracked_changed + new_changed) as u64)
        });
        let new_storage = block_storage
            .iter()
            .filter(|(addr, _)| !tracked_storage.contains_key(*addr))
            .filter_map(|(_, diff)| {
                let changed = diff
                    .slots
                    .values()
                    .filter(|(original, current)| original != current)
                    .count();
                DaEntry::for_storage(changed as u64)
            });

        let tracked_bytecodes = self.builder.deployed_bytecodes();
        let bytecodes = tracked_bytecodes
            .values()
            .chain(
                self.block
                    .deployed_bytecodes
                    .iter()
                    .filter(|(hash, _)| !tracked_bytecodes.contains_key(*hash))
                    .map(|(_, code)| code),
            )
            .map(|code| DaEntry::Bytecode {
                len: code.len() as u64,
            });

        merged_accounts
            .chain(new_accounts)
            .chain(merged_storage)
            .chain(new_storage)
            .chain(bytecodes)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, B256, KECCAK256_EMPTY, U256};
    use alpen_reth_statediff::{
        estimate_da_size, AccountSnapshot, BlockAccountChange, BlockStateChanges,
    };

    use super::*;

    fn addr(seed: u8) -> Address {
        Address::from([seed; 20])
    }

    fn hash(seed: u8) -> B256 {
        B256::from([seed; 32])
    }

    fn snapshot(balance: u64, nonce: u64, code_hash: B256) -> AccountSnapshot {
        AccountSnapshot {
            balance: U256::from(balance),
            nonce,
            code_hash,
        }
    }

    fn account(
        block: &mut BlockStateChanges,
        address: Address,
        original: Option<AccountSnapshot>,
        current: Option<AccountSnapshot>,
    ) {
        block
            .accounts
            .insert(address, BlockAccountChange { original, current });
    }

    fn slot(block: &mut BlockStateChanges, address: Address, key: u64, from: u64, to: u64) {
        block
            .storage
            .entry(address)
            .or_default()
            .slots
            .insert(U256::from(key), (U256::from(from), U256::from(to)));
    }

    /// Sort key so entry sets can be compared regardless of iteration order.
    fn sort_key(entry: &DaEntry) -> (u8, bool, bool, u64) {
        match *entry {
            DaEntry::Account {
                deleted,
                code_changed,
            } => (0, deleted, code_changed, 0),
            DaEntry::Storage { changed_slots } => (1, false, false, changed_slots),
            DaEntry::Bytecode { len } => (2, false, false, len),
        }
    }

    fn sorted_entries(state: &impl DaSizable) -> Vec<DaEntry> {
        let mut entries: Vec<_> = state.da_entries().collect();
        entries.sort_by_key(sort_key);
        entries
    }

    fn applied(builder: &BatchBuilder, block: &BlockStateChanges) -> BatchBuilder {
        let mut applied = builder.clone();
        applied.apply_block(block);
        applied
    }

    /// Pending batch: a hot EOA, a contract with two slots, and one deployed bytecode.
    fn pending_builder() -> (BatchBuilder, Address, Address, B256) {
        let eoa = addr(0x11);
        let contract = addr(0x22);
        let code_hash = hash(0xaa);

        let mut block = BlockStateChanges::new();
        account(
            &mut block,
            eoa,
            Some(snapshot(100, 0, KECCAK256_EMPTY)),
            Some(snapshot(90, 1, KECCAK256_EMPTY)),
        );
        account(&mut block, contract, None, Some(snapshot(0, 1, code_hash)));
        slot(&mut block, contract, 1, 0, 5);
        slot(&mut block, contract, 2, 0, 7);
        block
            .deployed_bytecodes
            .insert(code_hash, Bytes::from(vec![0x60u8; 64]));

        let mut builder = BatchBuilder::new();
        builder.apply_block(&block);
        (builder, eoa, contract, code_hash)
    }

    #[test]
    fn view_matches_clone_then_apply_for_overlapping_block() {
        let (builder, eoa, contract, code_hash) = pending_builder();

        // The next block touches the hot EOA again, updates one slot, adds a slot on
        // the contract, touches a new account with storage, and re-deploys the same code
        // plus a new one.
        let mut block = BlockStateChanges::new();
        account(
            &mut block,
            eoa,
            Some(snapshot(90, 1, KECCAK256_EMPTY)),
            Some(snapshot(80, 2, KECCAK256_EMPTY)),
        );
        slot(&mut block, contract, 1, 5, 6);
        slot(&mut block, contract, 3, 0, 9);
        let newcomer = addr(0x33);
        account(
            &mut block,
            newcomer,
            None,
            Some(snapshot(1, 0, KECCAK256_EMPTY)),
        );
        slot(&mut block, newcomer, 1, 0, 1);
        block
            .deployed_bytecodes
            .insert(code_hash, Bytes::from(vec![0x60u8; 64]));
        block
            .deployed_bytecodes
            .insert(hash(0xbb), Bytes::from(vec![0x60u8; 32]));

        let view = BatchWithBlock::new(&builder, &block);
        let expected = applied(&builder, &block);

        assert_eq!(sorted_entries(&view), sorted_entries(&expected));
        assert_eq!(estimate_da_size(&view), estimate_da_size(&expected));
        // The exact size is below the naive sum, which re-counts the shared entries.
        assert!(estimate_da_size(&view) < estimate_da_size(&builder) + estimate_da_size(&block));
    }

    #[test]
    fn view_discounts_reverts_to_batch_original() {
        let (builder, eoa, contract, _) = pending_builder();

        // Revert the EOA and one slot back to their pre-batch values, and delete the
        // contract that the batch created.
        let mut block = BlockStateChanges::new();
        account(
            &mut block,
            eoa,
            Some(snapshot(90, 1, KECCAK256_EMPTY)),
            Some(snapshot(100, 0, KECCAK256_EMPTY)),
        );
        account(&mut block, contract, Some(snapshot(0, 1, hash(0xaa))), None);
        slot(&mut block, contract, 1, 5, 0);

        let view = BatchWithBlock::new(&builder, &block);
        let expected = applied(&builder, &block);

        assert_eq!(sorted_entries(&view), sorted_entries(&expected));
        // Only slot 2 and the bytecode survive.
        assert_eq!(
            sorted_entries(&view),
            [
                DaEntry::Storage { changed_slots: 1 },
                DaEntry::Bytecode { len: 64 }
            ]
        );
    }

    #[test]
    fn view_of_empty_builder_is_the_block() {
        let mut block = BlockStateChanges::new();
        account(
            &mut block,
            addr(0x44),
            None,
            Some(snapshot(1, 1, KECCAK256_EMPTY)),
        );
        slot(&mut block, addr(0x44), 1, 0, 1);

        let builder = BatchBuilder::new();
        let view = BatchWithBlock::new(&builder, &block);
        assert_eq!(sorted_entries(&view), sorted_entries(&block));
    }
}
