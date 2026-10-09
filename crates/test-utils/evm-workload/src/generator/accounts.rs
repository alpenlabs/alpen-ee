//! Deterministic accounts and transaction signing.

use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
use alloy_primitives::{b256, keccak256, Address, Bytes, TxKind, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::Recovered;

/// A signed transaction with its sender, ready to execute.
pub(crate) type SignedTx = Recovered<TransactionSigned>;

/// Private key of Anvil's first dev account, the only account
/// `params/dev.json` funds.
const DEV_FUNDER_KEY: B256 =
    b256!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// Fee cap of every transaction. High enough to clear the base fee while the
/// full setup blocks push it up.
const MAX_FEE_PER_GAS: u128 = 50_000_000_000;

/// Priority fee of every EIP-1559 transaction.
const MAX_PRIORITY_FEE_PER_GAS: u128 = 1_000_000_000;

/// An externally owned account and its next nonce.
#[derive(Debug)]
pub(crate) struct Account {
    signer: PrivateKeySigner,
    nonce: u64,
}

impl Account {
    /// Anvil's first dev account.
    pub(crate) fn dev_funder() -> Self {
        Self::from_key(DEV_FUNDER_KEY)
    }

    /// The account at `index` in the pool derived from `seed`.
    pub(crate) fn derive(seed: u64, index: u64) -> Self {
        let mut preimage = Vec::with_capacity(32);
        preimage.extend_from_slice(b"alpen-evm-workload/eoa");
        preimage.extend_from_slice(&seed.to_be_bytes());
        preimage.extend_from_slice(&index.to_be_bytes());
        Self::from_key(keccak256(preimage))
    }

    fn from_key(key: B256) -> Self {
        Self {
            signer: PrivateKeySigner::from_bytes(&key).expect("valid secp256k1 key"),
            nonce: 0,
        }
    }

    pub(crate) fn address(&self) -> Address {
        self.signer.address()
    }

    /// Address of the contract this account's next transaction would create.
    pub(crate) fn next_create_address(&self) -> Address {
        self.address().create(self.nonce)
    }

    /// Signs an EIP-1559 transaction calling `to`, or creating a contract when
    /// `to` is `None`.
    pub(crate) fn eip1559(
        &mut self,
        chain_id: u64,
        to: Option<Address>,
        value: U256,
        input: Bytes,
        gas_limit: u64,
    ) -> SignedTx {
        let tx = TxEip1559 {
            chain_id,
            nonce: self.next_nonce(),
            gas_limit,
            max_fee_per_gas: MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
            to: to.map_or(TxKind::Create, TxKind::Call),
            value,
            access_list: Default::default(),
            input,
        };
        let signature = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("signing a hash with a local key cannot fail");
        let signed = TransactionSigned::from(tx.into_signed(signature));
        Recovered::new_unchecked(signed, self.address())
    }

    /// Signs a legacy (EIP-155) value transfer to `to`.
    pub(crate) fn legacy_transfer(
        &mut self,
        chain_id: u64,
        to: Address,
        value: U256,
        gas_limit: u64,
    ) -> SignedTx {
        let tx = TxLegacy {
            chain_id: Some(chain_id),
            nonce: self.next_nonce(),
            gas_price: MAX_FEE_PER_GAS,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: Bytes::new(),
        };
        let signature = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("signing a hash with a local key cannot fail");
        let signed = TransactionSigned::from(tx.into_signed(signature));
        Recovered::new_unchecked(signed, self.address())
    }

    fn next_nonce(&mut self) -> u64 {
        let nonce = self.nonce;
        self.nonce += 1;
        nonce
    }
}
