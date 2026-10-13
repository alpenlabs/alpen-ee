use strata_acct_types::{AccountId, BitcoinAmount, MsgPayload, SubjectId};
use strata_predicate::PredicateKey;

use crate::{
    ExecInputs, ExecNewPredicate, ExecOutputs, OutputMessage, OutputTransfer, SubjectDepositData,
};

impl ExecNewPredicate {
    /// Creates an empty declaration (no predicate rotation).
    pub fn new_empty() -> Self {
        Self {
            predicate: ssz_types::Optional::None,
        }
    }

    pub fn predicate(&self) -> Option<&PredicateKey> {
        match &self.predicate {
            ssz_types::Optional::Some(key) => Some(key),
            ssz_types::Optional::None => None,
        }
    }
}

impl From<Option<PredicateKey>> for ExecNewPredicate {
    fn from(key: Option<PredicateKey>) -> Self {
        Self {
            predicate: key.into(),
        }
    }
}

impl ExecInputs {
    fn new(subject_deposits: Vec<SubjectDepositData>) -> Self {
        Self {
            subject_deposits: subject_deposits
                .try_into()
                .expect("subject_deposits should not exceed capacity"),
        }
    }

    /// Creates a new empty instance.
    pub fn new_empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn subject_deposits(&self) -> &[SubjectDepositData] {
        self.subject_deposits.as_ref()
    }

    pub fn add_subject_deposit(&mut self, d: SubjectDepositData) {
        self.subject_deposits
            .push(d)
            .expect("subject_deposits list at capacity");
    }

    /// Appends `other`'s deposits after these ones.
    ///
    /// A chunk's inputs are its blocks' inputs joined this way, in block
    /// order.
    pub fn extend_from(&mut self, other: &ExecInputs) {
        for d in other.subject_deposits() {
            self.add_subject_deposit(d.clone());
        }
    }

    /// Returns the total number of inputs across all types.
    pub fn total_inputs(&self) -> usize {
        self.subject_deposits.len()
    }
}

impl SubjectDepositData {
    pub fn new(dest: SubjectId, value: BitcoinAmount) -> Self {
        Self { dest, value }
    }

    pub fn dest(&self) -> SubjectId {
        self.dest
    }

    pub fn value(&self) -> BitcoinAmount {
        self.value
    }
}

impl ExecOutputs {
    fn new(output_transfers: Vec<OutputTransfer>, output_messages: Vec<OutputMessage>) -> Self {
        Self {
            // TODO(STR-2172): propagate up the bounds checks here
            output_transfers: output_transfers
                .try_into()
                .expect("output_transfers should not exceed capacity"),
            output_messages: output_messages
                .try_into()
                .expect("output_messages should not exceed capacity"),
            new_predicate: ExecNewPredicate::new_empty(),
        }
    }

    /// Creates a new empty instance.
    pub fn new_empty() -> Self {
        Self::new(Vec::new(), Vec::new())
    }

    pub fn output_transfers(&self) -> &[OutputTransfer] {
        self.output_transfers.as_ref()
    }

    /// Adds a transfer output.
    pub fn add_transfer(&mut self, t: OutputTransfer) {
        // FIXME(STR-2172): remove expect
        self.output_transfers
            .push(t)
            .expect("chain/io: output_transfers list at capacity");
    }

    pub fn output_messages(&self) -> &[OutputMessage] {
        self.output_messages.as_ref()
    }

    /// Adds a message output.
    pub fn add_message(&mut self, m: OutputMessage) {
        // FIXME(STR-2172): remove expect
        self.output_messages
            .push(m)
            .expect("chain/io: output_messages list at capacity");
    }

    /// Sets the predicate rotation declared by this block.
    pub fn set_new_predicate(&mut self, key: Option<PredicateKey>) {
        self.new_predicate = key.into();
    }

    /// Returns the predicate rotation declared by this block, if any.
    pub fn new_predicate(&self) -> Option<&PredicateKey> {
        self.new_predicate.predicate()
    }

    /// Appends `other`'s transfers and messages after these ones, and takes
    /// its predicate rotation if it declares one.
    ///
    /// A chunk's outputs are its blocks' outputs joined this way, in block
    /// order. The guest and the hosts must join them the same way, or the
    /// chunk transition the host claims won't match the one the guest
    /// computes.
    ///
    /// If `other` declares no rotation, a rotation already recorded here stays.
    /// This doesn't stop a block from following a rotation. The chunk runtime
    /// rejects that case itself.
    pub fn extend_from(&mut self, other: &ExecOutputs) {
        for t in other.output_transfers() {
            self.add_transfer(t.clone());
        }
        for m in other.output_messages() {
            self.add_message(m.clone());
        }
        if let Some(key) = other.new_predicate() {
            self.set_new_predicate(Some(key.clone()));
        }
    }

    /// Checks whether these claimed outputs match the outputs that executing
    /// the block produced.
    ///
    /// `new_predicate` is deliberately left out: it's an account-level fact
    /// drawn from the pending-input queue, not something execution can derive,
    /// so executed outputs always leave it empty. Checking a declared rotation
    /// belongs to a cross-check over the chunk's blocks, not this per-block
    /// comparison.
    pub fn matches_executed(&self, executed: &ExecOutputs) -> bool {
        self.output_transfers() == executed.output_transfers()
            && self.output_messages() == executed.output_messages()
    }
}

impl OutputMessage {
    pub fn new(dest: AccountId, payload: MsgPayload) -> Self {
        Self { dest, payload }
    }

    pub fn dest(&self) -> AccountId {
        self.dest
    }

    pub fn payload(&self) -> &MsgPayload {
        &self.payload
    }
}

impl OutputTransfer {
    pub fn new(dest: AccountId, value: BitcoinAmount) -> Self {
        Self { dest, value }
    }

    pub fn dest(&self) -> AccountId {
        self.dest
    }

    pub fn value(&self) -> BitcoinAmount {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use strata_identifiers::Hash;
    use strata_test_utils_ssz::ssz_proptest;

    use super::*;
    use crate::*;

    /// Satoshi amounts capped at the bitcoin money supply, the bound
    /// [`BitcoinAmount`] enforces.
    fn any_sats() -> impl Strategy<Value = u64> {
        0..=21_000_000u64 * 100_000_000
    }

    mod exec_block_commitment {
        use super::*;

        ssz_proptest!(
            ExecBlockCommitment,
            (any::<[u8; 32]>(), any::<[u8; 32]>()).prop_map(|(blkid, hash)| {
                ExecBlockCommitment {
                    exec_blkid: blkid.into(),
                    raw_block_encoded_hash: hash.into(),
                }
            })
        );

        #[test]
        fn test_new() {
            let blkid = Hash::new([0xaa; 32]);
            let hash = Hash::new([0xbb; 32]);
            let commitment = ExecBlockCommitment::new(blkid, hash);

            assert_eq!(commitment.exec_blkid(), blkid);
            assert_eq!(commitment.raw_block_encoded_hash(), hash);
        }
    }

    mod subject_deposit_data {
        use super::*;

        ssz_proptest!(
            SubjectDepositData,
            (any::<[u8; 32]>(), any_sats()).prop_map(|(dest, sats)| {
                SubjectDepositData {
                    dest: SubjectId::new(dest),
                    value: BitcoinAmount::try_from(sats).unwrap(),
                }
            })
        );

        #[test]
        fn test_new() {
            let dest = SubjectId::new([0xcc; 32]);
            let value = BitcoinAmount::try_from(1000).unwrap();
            let deposit = SubjectDepositData::new(dest, value);

            assert_eq!(deposit.dest(), dest);
            assert_eq!(deposit.value(), value);
        }
    }

    mod block_inputs {
        use super::*;

        ssz_proptest!(
            ExecInputs,
            prop::collection::vec(
                (any::<[u8; 32]>(), any_sats()).prop_map(|(dest, sats)| {
                    SubjectDepositData {
                        dest: SubjectId::new(dest),
                        value: BitcoinAmount::try_from(sats).unwrap(),
                    }
                }),
                0..10
            )
            .prop_map(|deposits| ExecInputs {
                subject_deposits: deposits
                    .try_into()
                    .expect("subject_deposits should not exceed capacity"),
            })
        );

        #[test]
        fn test_new_empty() {
            let inputs = ExecInputs::new_empty();
            assert_eq!(inputs.total_inputs(), 0);
        }

        #[test]
        fn test_add_subject_deposit() {
            let mut inputs = ExecInputs::new_empty();
            let deposit = SubjectDepositData::new(
                SubjectId::new([0xdd; 32]),
                BitcoinAmount::try_from(500).unwrap(),
            );

            inputs.add_subject_deposit(deposit);
            assert_eq!(inputs.total_inputs(), 1);
        }
    }

    mod output_transfer {
        use super::*;

        ssz_proptest!(
            OutputTransfer,
            (any::<[u8; 32]>(), any_sats()).prop_map(|(dest, sats)| {
                OutputTransfer {
                    dest: AccountId::new(dest),
                    value: BitcoinAmount::try_from(sats).unwrap(),
                }
            })
        );

        #[test]
        fn test_new() {
            let dest = AccountId::new([0xee; 32]);
            let value = BitcoinAmount::try_from(2000).unwrap();
            let transfer = OutputTransfer::new(dest, value);

            assert_eq!(transfer.dest(), dest);
            assert_eq!(transfer.value(), value);
        }
    }

    mod block_outputs {
        use strata_predicate::{PredicateKey, PredicateTypeId};

        use super::*;

        fn predicate_key_strategy() -> impl Strategy<Value = PredicateKey> {
            prop::collection::vec(any::<u8>(), 0..64).prop_map(|condition| {
                PredicateKey::try_new(PredicateTypeId::AlwaysAccept, condition)
                    .expect("condition fits within the key length limit")
            })
        }

        ssz_proptest!(
            ExecOutputs,
            (
                prop::collection::vec(
                    (any::<[u8; 32]>(), any_sats()).prop_map(|(dest, sats)| {
                        OutputTransfer {
                            dest: AccountId::new(dest),
                            value: BitcoinAmount::try_from(sats).unwrap(),
                        }
                    }),
                    0..10
                ),
                prop::collection::vec(
                    (
                        any::<[u8; 32]>(),
                        any_sats(),
                        prop::collection::vec(any::<u8>(), 0..50)
                    )
                        .prop_map(|(dest, sats, data)| {
                            OutputMessage::new(
                                AccountId::new(dest),
                                strata_acct_types::MsgPayload::from_bytes(
                                    BitcoinAmount::try_from(sats).unwrap(),
                                    data,
                                )
                                .expect("message payload bytes must fit within SSZ max length"),
                            )
                        }),
                    0..10
                ),
                prop::option::of(predicate_key_strategy())
            )
                .prop_map(|(transfers, messages, new_predicate)| {
                    ExecOutputs {
                        output_transfers: transfers
                            .try_into()
                            .expect("output_transfers should not exceed capacity"),
                        output_messages: messages
                            .try_into()
                            .expect("output_messages should not exceed capacity"),
                        new_predicate: new_predicate.into(),
                    }
                })
        );

        #[test]
        fn test_new_empty() {
            let outputs = ExecOutputs::new_empty();
            assert_eq!(outputs.output_transfers().len(), 0);
            assert_eq!(outputs.output_messages().len(), 0);
            assert_eq!(outputs.new_predicate(), None);
        }

        #[test]
        fn matches_executed_compares_transfers_and_messages() {
            let executed = ExecOutputs::new_empty();

            let mut extra_transfer = ExecOutputs::new_empty();
            extra_transfer.add_transfer(OutputTransfer::new(
                AccountId::new([1; 32]),
                BitcoinAmount::try_from(5u64).unwrap(),
            ));
            assert!(!extra_transfer.matches_executed(&executed));

            let mut extra_message = ExecOutputs::new_empty();
            extra_message.add_message(OutputMessage::new(
                AccountId::new([2; 32]),
                MsgPayload::from_bytes(BitcoinAmount::try_from(5u64).unwrap(), vec![]).unwrap(),
            ));
            assert!(!extra_message.matches_executed(&executed));
        }

        #[test]
        fn extend_from_appends_in_order_and_keeps_a_rotation() {
            let transfer = |n: u8| {
                OutputTransfer::new(
                    AccountId::new([n; 32]),
                    BitcoinAmount::try_from(u64::from(n)).unwrap(),
                )
            };

            let mut first = ExecOutputs::new_empty();
            first.add_transfer(transfer(1));
            first.set_new_predicate(Some(PredicateKey::always_accept()));
            let mut second = ExecOutputs::new_empty();
            second.add_transfer(transfer(2));

            let mut joined = ExecOutputs::new_empty();
            joined.extend_from(&first);
            joined.extend_from(&second);

            assert_eq!(joined.output_transfers(), &[transfer(1), transfer(2)]);
            assert_eq!(joined.new_predicate(), Some(&PredicateKey::always_accept()));
        }

        #[test]
        fn matches_executed_ignores_new_predicate() {
            let mut claimed = ExecOutputs::new_empty();
            claimed.set_new_predicate(Some(PredicateKey::always_accept()));
            assert!(claimed.matches_executed(&ExecOutputs::new_empty()));
        }
    }
}
