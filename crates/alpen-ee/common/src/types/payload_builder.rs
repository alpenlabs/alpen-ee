use alloy_primitives::{Address, B256};
use alpen_ee_params::AlpenSpecId;
use strata_acct_types::BitcoinAmount;

/// Inputs to control evm block builder.
#[derive(Debug, Clone)]
pub struct PayloadBuildAttributes {
    /// blockhash of parent block for new block.
    parent: B256,
    /// timestamp of the new block.
    timestamp: u64,
    /// deposits to be included in the new block.
    deposits: Vec<DepositInfo>,
    /// Alpen spec version governing the new block.
    spec_version: AlpenSpecId,
}

impl PayloadBuildAttributes {
    pub fn new(
        parent: B256,
        timestamp: u64,
        deposits: Vec<DepositInfo>,
        spec_version: AlpenSpecId,
    ) -> Self {
        Self {
            parent,
            timestamp,
            deposits,
            spec_version,
        }
    }

    pub fn parent(&self) -> B256 {
        self.parent
    }

    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn deposits(&self) -> &[DepositInfo] {
        &self.deposits
    }

    pub fn spec_version(&self) -> AlpenSpecId {
        self.spec_version
    }
}

/// Describes an incoming deposit that should be minted.
#[derive(Debug, Clone)]
pub struct DepositInfo {
    /// Unique index for this deposit.
    idx: u64,
    /// Address inside evm chain where the deposit should be minted to.
    address: Address,
    /// Amount that has been deposited.
    amount: BitcoinAmount,
}

impl DepositInfo {
    pub fn new(idx: u64, address: Address, amount: BitcoinAmount) -> Self {
        Self {
            idx,
            address,
            amount,
        }
    }

    pub fn idx(&self) -> u64 {
        self.idx
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn amount(&self) -> BitcoinAmount {
        self.amount
    }
}
