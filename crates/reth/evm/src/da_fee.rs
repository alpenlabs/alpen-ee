//! Per-transaction data-availability (DA) fee sizing.
//!
//! The DA fee charges a transaction for the Bitcoin data-availability cost of its
//! state diff: `da_fee = da_rate_wei_per_byte * diff_size`. This module owns the
//! single, deterministic routine that computes `diff_size` for one transaction from
//! the EVM state change-set.
//!
//! Determinism is the core requirement: the sequencer, a re-executing full node, and
//! the chunk proof must all compute the identical `diff_size` (and therefore the
//! identical post-charge balances / state root). [`calc_diff_size`] is a pure,
//! order-independent integer sum over the change-set — fixed per-field byte costs plus the
//! length of any deployed bytecode — so it reproduces exactly across all three.
//!
//! This routine is intended to be the single source of truth for DA sizing — the
//! in-EVM charge and the fee-estimation RPCs must both call it, so a quote can never
//! disagree with the charge.
//!
//! TODO(STR-4226): Refactor and move it to a separate crate outside of reth so that
//! all the components (rpc, execution, guest) can access it without necessarily
//! depending on heavy reth crates.

use alpen_ee_params::{AlpenSpecId, HeaderExtra};
use reth_evm::{eth::EthEvmContext, Database};
use revm::state::EvmState;
use revm_primitives::{Bytes, KECCAK_EMPTY, U256};

use crate::utils::WEI_PER_SAT;

// The constants below are deliberate **upper bounds** on the DA-encoded size of each
// field (`statediff` `AccountDiff` / `StorageDiff` and their codecs). The DA charge must
// never *underestimate* the bytes a transaction pushes to L1 — undercharging means the
// protocol silently subsidizes DA — so every field is charged its worst case regardless of
// the actual (often smaller, trimmed) encoding. Overcharging is the accepted trade-off.
//
// KNOWN EXCEPTION — contract creations / code writes are currently *under*-counted, so
// `calc_diff_size` is not a strict upper bound for them. The separate `deployed_bytecodes`
// map-entry framing and the per-account `AccountChange` discriminant are not charged; see
// the code-write branch in [`calc_diff_size`] for the exact missing bytes and rationale.
// This leaves a small, bounded DA subsidy on deployments, deliberately deferred for now.

/// Worst-case byte cost of an account/slot address key in a DA map: the fixed 20-byte
/// address (`CodecAddress`).
const ACCOUNT_KEY_BYTES: u64 = 20;

/// Worst-case byte cost of a touched account's info delta in `AccountDiff`.
///
/// A 1-byte compound header, plus (when changed) a signed balance delta and a signed nonce
/// delta. Upper bounds from the codecs: balance `SignedU256Delta` = 1 tag + 1 length + 32
/// value = 34; nonce `SignedVarInt` over `u64` = 10 (`MAX_VARINT_BYTES`); header = 1. The
/// code-hash register is *not* included here — it is added separately only when the code
/// changes ([`CODE_HASH_BYTES`]) — so a merely-called contract costs exactly an EOA.
const ACCOUNT_INFO_BYTES: u64 = 1 + 34 + 10;

/// Worst-case byte cost of the code-hash register, written to DA whenever a transaction
/// changes an account's code — contract *creation* or an EIP-7702 delegation: the fixed
/// 32-byte `CodecB256`. Added together with the written bytecode length (see
/// [`calc_diff_size`]); never charged for a plain contract call, which leaves code unchanged.
const CODE_HASH_BYTES: u64 = 32;

/// Worst-case byte cost of a changed storage slot's key: the fixed 32-byte big-endian slot
/// key (stored untrimmed).
const SLOT_KEY_BYTES: u64 = 32;

/// Worst-case byte cost of a changed storage slot's value: `TrimmedStorageValue` = 1
/// length byte + up to 32 value bytes.
const SLOT_VALUE_BYTES: u64 = 1 + 32;

/// Worst-case per-account framing for accounts that change storage. Storage lives in a
/// second, address-keyed map (`StorageDiff`), so the account re-encodes its 20-byte
/// address there, plus that entry's slot-count prefix (`u32`, at most 5 bytes).
const STORAGE_ACCOUNT_OVERHEAD_BYTES: u64 = ACCOUNT_KEY_BYTES + 5;

/// DA-coverage report values written by the charge handler into the shared report cell
/// and read by the sequencer's payload builder for tx admission.
///
/// The default / reset value is [`DA_COVERAGE_UNKNOWN`] (`0`) — deliberately *not* `OK` —
/// so a cell that was never written for the current transaction (e.g. a zero-fee system
/// call that skips the charge, or a stale value from a previous tx) is never mistaken for
/// "covered". The payload builder skips a transaction only on an explicit
/// [`DA_COVERAGE_CAPPED`]; `UNKNOWN` and `OK` both mean "include".
pub const DA_COVERAGE_UNKNOWN: u64 = 0;
/// The transaction's DA fee was fully covered by its unused authorized gas.
pub const DA_COVERAGE_OK: u64 = 1;
/// The transaction's DA fee was capped by its unused authorized gas — under-covered, so
/// the protocol would subsidize it. The payload builder skips such transactions.
pub const DA_COVERAGE_CAPPED: u64 = 2;

/// Computes the DA `diff_size` (in bytes) for a single transaction's state change-set.
///
/// `state` is the post-execution [`EvmState`] for the transaction. Every touched account
/// contributes its key and its trimmed balance/nonce delta; a transaction that changes an
/// account's code — contract *creation* or an EIP-7702 delegation — additionally contributes
/// the code-hash register and the length of the written bytecode; and every changed storage
/// slot contributes a fixed key/value cost. The costs are summed directly — no discount,
/// compression, or fixed-overhead adjustment is applied.
///
/// The result is deterministic and independent of map iteration order.
pub fn calc_diff_size(state: &EvmState) -> u64 {
    let mut diff_size: u64 = 0;

    for account in state.values() {
        // Only accounts that were actually touched enter the state diff.
        if !account.is_touched() {
            continue;
        }

        // Balance + nonce deltas, encoded the same way for every touched account.
        diff_size = diff_size.saturating_add(ACCOUNT_KEY_BYTES + ACCOUNT_INFO_BYTES);

        // The code-hash register and deployed bytecode reach L1 DA whenever a transaction
        // changes an account's code. Two cases produce that in one transaction:
        //   * contract *creation* (`is_created`), which writes freshly deployed bytecode;
        //   * an EIP-7702 authorization, which installs a delegation designator on an existing
        //     (non-created) account — revm records it as a touched account whose code is an
        //     `Eip7702` bytecode, so `is_created` is NOT set. Alpen enables Prague at genesis and
        //     accepts type-4 txs, so this path is live.
        // Charge both so a type-4 authorization is never under-sized; a plain call to an
        // existing contract leaves the code unchanged and is charged for neither.
        //
        // This sizes on the *presence* of created / delegation code, not a diff against the
        // pre-transaction hash — the per-transaction change-set does not carry the original
        // hash. It therefore over-charges only the rare tx sent from an already-delegated
        // account whose delegation is unchanged (a bounded `CODE_HASH_BYTES` + 23 designator
        // bytes) and never under-charges. Overcharging is the accepted trade-off.
        let code_written = match &account.info.code {
            Some(code) if code.is_eip7702() => true,
            Some(_) | None => account.is_created() && account.info.code_hash != KECCAK_EMPTY,
        };
        if code_written {
            diff_size = diff_size.saturating_add(CODE_HASH_BYTES);
            if let Some(code) = &account.info.code {
                diff_size = diff_size.saturating_add(code.original_byte_slice().len() as u64);
            }

            // KNOWN UNDERCOUNT (deliberate, deferred): this charges the account's code-hash
            // register and the raw bytecode length, but NOT the framing of the separate
            // `deployed_bytecodes` map entry that also carries the bytecode to L1 — a second
            // 32-byte `CodecB256` code-hash key plus its `u32` (4-byte) length prefix. Nor is
            // the 1-byte `AccountChange` discriminant on every account entry counted. So for a
            // creation with a large balance delta — where the `ACCOUNT_INFO_BYTES` worst-case
            // slack can't absorb the omission — the estimate falls a few dozen bytes short of
            // the bytes actually posted, i.e. a small, bounded DA subsidy on deployments.
            // Left uncharged for now to keep the per-tx estimate simple; revisit alongside the
            // sizing refactor (see the module `TODO(STR-4226)`).
        }

        // Changed storage slots live in a second, address-keyed map, so an account with any
        // changed slot pays the storage-map framing once plus each slot's key/value.
        let changed_slots = account
            .storage
            .values()
            .filter(|slot| slot.is_changed())
            .count() as u64;
        if changed_slots > 0 {
            diff_size = diff_size.saturating_add(STORAGE_ACCOUNT_OVERHEAD_BYTES);
            diff_size = diff_size
                .saturating_add(changed_slots.saturating_mul(SLOT_KEY_BYTES + SLOT_VALUE_BYTES));
        }
    }

    diff_size
}

/// Decodes the per-block DA rate (wei per byte) from the EVM header `extra_data`.
///
/// The rate is a body field of the versioned [`HeaderExtra`] layout, so it is read through
/// that codec rather than off a fixed offset. Anything that does not decode under the
/// layout (the genesis label, an empty field, a corrupt stamp) yields `0`, which disables
/// the DA charge — so the charge stays dormant until a rate is committed.
///
/// Header validation is what rejects malformed `extra_data`; by the time a block reaches
/// execution its layout has already been checked, so falling back to `0` here is a
/// belt-and-braces default rather than a policy decision.
pub fn da_rate_from_extra_data(extra_data: &Bytes) -> u64 {
    HeaderExtra::decode(extra_data)
        .map(|extra| extra.da_rate())
        .unwrap_or(0)
}

/// Encodes a block's spec version and DA rate into `extra_data` bytes.
pub fn da_rate_to_extra_data(spec_version: AlpenSpecId, da_rate: u64) -> Bytes {
    Bytes::from(HeaderExtra::new(spec_version, da_rate).encode())
}

/// SegWit witness discount: DA payload rides in witness data, weighted at 1/4 of a vByte.
const SEGWIT_WITNESS_DIVISOR: u64 = 4;

/// Default Bitcoin fee rate (sat/vByte) the live DA rate is seeded from when no explicit
/// `ALPEN_DA_RATE_WEI_PER_BYTE` override is set.
///
/// 4 sat/vByte is a conservative normal-conditions rate; after the SegWit witness discount
/// it is exactly 1 satoshi per DA byte. It is only a seed — the live rate is expected to
/// track the sequencer's actual Bitcoin fee rate later (see [`btc_fee_rate_to_da_rate`]).
pub const DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE: u64 = 4;

/// Default live DA rate (wei per DA byte): [`DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE`] mapped
/// through the SegWit witness discount (`WEI_PER_SAT` wei, i.e. 1 sat per DA byte).
pub const DEFAULT_DA_RATE_WEI_PER_BYTE: u64 =
    DEFAULT_DA_BTC_FEE_RATE_SAT_PER_VBYTE * WEI_PER_SAT / SEGWIT_WITNESS_DIVISOR;

/// Converts a Bitcoin fee rate (satoshis per virtual byte) to the DA rate (wei per byte).
///
/// `da_rate = btc_fee_rate[sat/vB] * 10^10[wei/sat] / 4` (the SegWit witness discount).
///
/// NOTE: for now this reuses the sequencer's Bitcoin publication fee rate
/// (`btcio::writer::fees::resolve_fee_rate`). The DA fee-model rate is expected to be
/// decoupled from the publication rate — and smoothed/cached — in a later revision.
pub fn btc_fee_rate_to_da_rate(sat_per_vbyte: u64) -> u64 {
    sat_per_vbyte
        .saturating_mul(WEI_PER_SAT)
        .saturating_div(SEGWIT_WITNESS_DIVISOR)
}

/// Computes the DA fee to charge, bounded by the caller's unused authorized gas value.
///
/// The raw fee is `da_rate * diff_size`, capped at `remaining_value` — the value of the
/// gas the caller authorized (prepaid) but did not consume.
///
/// # Policy: cap, never fail
///
/// When the raw fee exceeds `remaining_value` (the committed DA rate rose, or the diff
/// came out larger than the estimate the wallet quoted its `effective_gas` against), this
/// **caps at `remaining_value` and the transaction still succeeds** — it never fails and
/// never charges beyond what the signature authorized. The bound also guarantees the debit
/// is always covered: the caller was just refunded `remaining_value`, so a charge `<=` it
/// cannot fail. The cost of this choice is that the **protocol undercharges (subsidizes)**
/// the shortfall in that case, rather than reverting the transaction.
///
/// This is acceptable for v1 because the quote-to-inclusion window is short (seconds) and
/// the committed rate moves per block, so the shortfall is rare and small. It is *not*
/// turned into an out-of-gas failure: DA is a byte-priced charge, not gas-metered, so a
/// clean OOG would require reverting a fully-executed transaction post-execution (and the
/// forfeited value would go to the coinbase regardless).
///
/// NOTE(fee-model): reduce the subsidy without failing — quote `effective_gas` at
/// `da_rate * (1 + margin)` in the fee RPC (safety margin), and/or add a per-block
/// rate-change bound so the committed rate cannot rise faster than the quoted headroom.
pub fn bounded_da_fee(da_rate: U256, diff_size: u64, remaining_value: U256) -> U256 {
    da_rate
        .saturating_mul(U256::from(diff_size))
        .min(remaining_value)
}

/// Read access to the raw EVM state change-set on the concrete EVM context.
///
/// The generic revm `Handler` cannot reach the whole `EvmState` through `JournalTr`, but
/// the concrete [`EthEvmContext`] exposes it via its journal. The DA charge binds to this
/// trait so it can size the diff in the handler. The fee itself is applied through the
/// journal (`load_account_mut`), not by mutating this map, so accounts stay loaded.
pub trait DaStateAccess {
    /// Returns the transaction's state change-set.
    fn evm_state(&self) -> &EvmState;
}

impl<DB: Database> DaStateAccess for EthEvmContext<DB> {
    fn evm_state(&self) -> &EvmState {
        &self.journaled_state.state
    }
}

#[cfg(test)]
mod tests {
    use revm::state::{Account, AccountInfo, Bytecode, EvmStorageSlot};
    use revm_primitives::{Address, B256, U256};

    use super::*;

    fn touched_eoa() -> Account {
        let mut account = Account::from(AccountInfo::default());
        account.mark_touch();
        account
    }

    fn touched_contract() -> Account {
        let info = AccountInfo {
            code_hash: B256::repeat_byte(0x11),
            ..AccountInfo::default()
        };
        let mut account = Account::from(info);
        account.mark_touch();
        account
    }

    /// A contract created in this transaction, carrying `code` as its deployed bytecode.
    fn created_contract(code: &[u8]) -> Account {
        let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(code));
        let info = AccountInfo {
            code_hash: bytecode.hash_slow(),
            code: Some(bytecode),
            ..AccountInfo::default()
        };
        let mut account = Account::from(info);
        account.mark_touch();
        account.mark_created();
        account
    }

    /// An existing account that received an EIP-7702 delegation designator this
    /// transaction: touched (nonce/balance changed) but NOT created, carrying `Eip7702`
    /// code — the shape revm produces for a type-4 authorization on a live account.
    fn delegated_eoa() -> Account {
        let code = Bytecode::new_eip7702(Address::repeat_byte(0x42));
        let info = AccountInfo {
            code_hash: code.hash_slow(),
            code: Some(code),
            ..AccountInfo::default()
        };
        let mut account = Account::from(info);
        account.mark_touch();
        account
    }

    fn state_of(accounts: impl IntoIterator<Item = (Address, Account)>) -> EvmState {
        let mut state = EvmState::default();
        for (addr, account) in accounts {
            state.insert(addr, account);
        }
        state
    }

    /// The worst-case bytes charged for a single touched account with no storage,
    /// creation, or code: address key + account-info delta.
    const ACCOUNT_BASE: u64 = ACCOUNT_KEY_BYTES + ACCOUNT_INFO_BYTES;

    #[test]
    fn empty_state_is_zero() {
        assert_eq!(calc_diff_size(&EvmState::default()), 0);
    }

    #[test]
    fn untouched_accounts_are_ignored() {
        // An account present but not touched must not contribute.
        let state = state_of([(
            Address::repeat_byte(1),
            Account::from(AccountInfo::default()),
        )]);
        assert_eq!(calc_diff_size(&state), 0);
    }

    #[test]
    fn single_eoa() {
        let state = state_of([(Address::repeat_byte(1), touched_eoa())]);
        assert_eq!(calc_diff_size(&state), ACCOUNT_BASE);
    }

    #[test]
    fn touched_contract_costs_same_as_eoa() {
        // A contract merely called (touched, not created) re-encodes only its balance and
        // nonce deltas — the code-hash register is unset — so it costs exactly an EOA.
        let eoa = state_of([(Address::repeat_byte(1), touched_eoa())]);
        let contract = state_of([(Address::repeat_byte(1), touched_contract())]);
        assert_eq!(calc_diff_size(&contract), calc_diff_size(&eoa));
        assert_eq!(calc_diff_size(&contract), ACCOUNT_BASE);
    }

    #[test]
    fn created_contract_charges_code_hash_and_bytecode() {
        let code = [0xabu8; 100];
        let state = state_of([(Address::repeat_byte(1), created_contract(&code))]);
        // Account entry + the code-hash register + every byte of the deployed bytecode.
        assert_eq!(
            calc_diff_size(&state),
            ACCOUNT_BASE + CODE_HASH_BYTES + code.len() as u64
        );
    }

    #[test]
    fn eip7702_delegation_charges_code_hash_and_designator() {
        // A type-4 authorization installs a delegation designator on an existing account
        // without marking it created; it must still be charged the code-hash register plus
        // the designator bytes (regression against the old `is_created`-only gate, which
        // charged this account nothing and so under-sized the DA).
        let state = state_of([(Address::repeat_byte(1), delegated_eoa())]);
        let designator_len = Bytecode::new_eip7702(Address::repeat_byte(0x42))
            .original_byte_slice()
            .len() as u64;
        assert_eq!(
            calc_diff_size(&state),
            ACCOUNT_BASE + CODE_HASH_BYTES + designator_len
        );
        assert!(calc_diff_size(&state) > ACCOUNT_BASE);
    }

    #[test]
    fn changed_storage_counts_unchanged_does_not() {
        let mut account = touched_eoa();
        // changed slot: original != present
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), 0),
        );
        // unchanged slot: original == present
        account
            .storage
            .insert(U256::from(2), EvmStorageSlot::new(U256::from(7), 0));

        let state = state_of([(Address::repeat_byte(1), account)]);
        // Account base + the storage-map framing + one changed slot's key/value.
        assert_eq!(
            calc_diff_size(&state),
            ACCOUNT_BASE + STORAGE_ACCOUNT_OVERHEAD_BYTES + (SLOT_KEY_BYTES + SLOT_VALUE_BYTES)
        );
    }

    #[test]
    fn storage_value_is_charged_at_full_word_worst_case() {
        // A full 32-byte storage value must not be charged less than the constant: the
        // charge is value-independent, so it never underestimates a large slot value.
        let mut small = touched_eoa();
        small.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), 0),
        );
        let mut full = touched_eoa();
        full.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::MAX, 0),
        );
        let small_state = state_of([(Address::repeat_byte(1), small)]);
        let full_state = state_of([(Address::repeat_byte(1), full)]);
        assert_eq!(calc_diff_size(&small_state), calc_diff_size(&full_state));
        assert_eq!(
            calc_diff_size(&full_state),
            ACCOUNT_BASE + STORAGE_ACCOUNT_OVERHEAD_BYTES + (SLOT_KEY_BYTES + SLOT_VALUE_BYTES)
        );
    }

    #[test]
    fn deterministic_regardless_of_insertion_order() {
        let a = (Address::repeat_byte(1), touched_eoa());
        let b = (Address::repeat_byte(2), touched_contract());
        let forward = state_of([a.clone(), b.clone()]);
        let backward = state_of([b, a]);
        assert_eq!(calc_diff_size(&forward), calc_diff_size(&backward));
    }

    #[test]
    fn da_rate_extra_data_roundtrips() {
        let rate = 2_500_000_000_u64;
        for version in [AlpenSpecId::V0, AlpenSpecId::V1] {
            assert_eq!(
                da_rate_from_extra_data(&da_rate_to_extra_data(version, rate)),
                rate,
                "{version:?}"
            );
        }
    }

    #[test]
    fn undecodable_extra_data_yields_no_rate() {
        // Genesis label / empty field / truncated stamp => no rate => charge is dormant.
        assert_eq!(da_rate_from_extra_data(&Bytes::from_static(b"SC")), 0);
        assert_eq!(da_rate_from_extra_data(&Bytes::new()), 0);
        assert_eq!(da_rate_from_extra_data(&Bytes::from_static(&[0x00, 0x00])), 0);
    }

    #[test]
    fn bounded_da_fee_caps_at_remaining_value() {
        let da_rate = U256::from(1_000u64);
        // raw = 1000 * 50 = 50_000; budget 60_000 => full fee charged.
        assert_eq!(
            bounded_da_fee(da_rate, 50, U256::from(60_000u64)),
            U256::from(50_000u64)
        );
        // raw = 50_000 but budget only 20_000 => capped (undercharge, never overcharge).
        assert_eq!(
            bounded_da_fee(da_rate, 50, U256::from(20_000u64)),
            U256::from(20_000u64)
        );
        // zero rate or zero budget => zero fee.
        assert_eq!(
            bounded_da_fee(U256::ZERO, 50, U256::from(60_000u64)),
            U256::ZERO
        );
        assert_eq!(bounded_da_fee(da_rate, 50, U256::ZERO), U256::ZERO);
    }
}
