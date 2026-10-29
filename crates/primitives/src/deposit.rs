use alloy_primitives::Address;
use strata_btc_types::BitcoinAmount;
use strata_identifiers::SubjectId;

use crate::{sats_to_gwei, subject_to_address_unchecked};

/// What a deposit mints in the EVM.
///
/// Alpen mints deposits through the EIP-4895 withdrawals field. The block builder and the
/// block verifier must both derive each withdrawal's address and amount from this type,
/// or they can disagree on a deposit and the verifier rejects every block that consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositMint {
    address: Address,
    amount_gwei: u64,
}

impl DepositMint {
    /// Maps a deposit of `amount` to `dest` to its mint.
    ///
    /// The address is the last 20 bytes of `dest`, even when the first 12 bytes are not
    /// zero. See [`subject_to_address_unchecked`] for why.
    ///
    /// Returns [`None`] if `amount` overflows gwei.
    pub fn from_deposit(dest: &SubjectId, amount: BitcoinAmount) -> Option<Self> {
        Some(Self {
            address: subject_to_address_unchecked(dest),
            amount_gwei: sats_to_gwei(amount.to_sat())?,
        })
    }

    /// Returns the address the deposit mints to.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Returns the minted amount in gwei, as EIP-4895 withdrawals carry it.
    pub fn amount_gwei(&self) -> u64 {
        self.amount_gwei
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::Amount;

    use super::*;

    #[test]
    fn test_deposit_mint_truncates_non_zero_padding() {
        let mut subject_bytes = [0xff; 32];
        subject_bytes[12..].copy_from_slice(&[0xaa; 20]);

        let amount = BitcoinAmount::try_from(10).expect("within the money supply");
        let mint = DepositMint::from_deposit(&SubjectId::new(subject_bytes), amount)
            .expect("fits in gwei");

        assert_eq!(mint.address(), Address::from([0xaa; 20]));
        assert_eq!(mint.amount_gwei(), 100);
    }

    #[test]
    fn test_deposit_mint_rejects_gwei_overflow() {
        // `From<Amount>` skips the money-supply check, so this is the only way to get
        // an amount large enough to overflow.
        let amount = BitcoinAmount::from(Amount::from_sat(u64::MAX));
        assert_eq!(
            DepositMint::from_deposit(&SubjectId::new([0; 32]), amount),
            None
        );
    }
}
