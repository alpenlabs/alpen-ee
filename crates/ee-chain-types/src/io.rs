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

    /// Creates a declaration rotating to the given predicate key.
    pub fn new_with_key(key: PredicateKey) -> Self {
        Self {
            predicate: ssz_types::Optional::Some(key),
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

    /// Sets the predicate rotation declared by this block, consuming and
    /// returning self.
    pub fn with_new_predicate(mut self, key: PredicateKey) -> Self {
        self.new_predicate = ExecNewPredicate::new_with_key(key);
        self
    }

    /// Sets the predicate rotation declared by this block.
    pub fn set_new_predicate(&mut self, key: Option<PredicateKey>) {
        self.new_predicate = key.into();
    }

    /// Returns the predicate rotation declared by this block, if any.
    pub fn new_predicate(&self) -> Option<&PredicateKey> {
        self.new_predicate.predicate()
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
            (any::<[u8; 32]>(), any::<u64>()).prop_map(|(dest, sats)| {
                SubjectDepositData {
                    dest: SubjectId::new(dest),
                    value: BitcoinAmount::from_sat(sats),
                }
            })
        );

        #[test]
        fn test_new() {
            let dest = SubjectId::new([0xcc; 32]);
            let value = BitcoinAmount::from_sat(1000);
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
                (any::<[u8; 32]>(), any::<u64>()).prop_map(|(dest, sats)| {
                    SubjectDepositData {
                        dest: SubjectId::new(dest),
                        value: BitcoinAmount::from_sat(sats),
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
            let deposit =
                SubjectDepositData::new(SubjectId::new([0xdd; 32]), BitcoinAmount::from_sat(500));

            inputs.add_subject_deposit(deposit);
            assert_eq!(inputs.total_inputs(), 1);
        }
    }

    mod output_transfer {
        use super::*;

        ssz_proptest!(
            OutputTransfer,
            (any::<[u8; 32]>(), any::<u64>()).prop_map(|(dest, sats)| {
                OutputTransfer {
                    dest: AccountId::new(dest),
                    value: BitcoinAmount::from_sat(sats),
                }
            })
        );

        #[test]
        fn test_new() {
            let dest = AccountId::new([0xee; 32]);
            let value = BitcoinAmount::from_sat(2000);
            let transfer = OutputTransfer::new(dest, value);

            assert_eq!(transfer.dest(), dest);
            assert_eq!(transfer.value(), value);
        }
    }

    mod block_outputs {
        use strata_predicate::{PredicateKey, PredicateTypeId};

        use super::*;

        fn predicate_key_strategy() -> impl Strategy<Value = PredicateKey> {
            prop::collection::vec(any::<u8>(), 0..64)
                .prop_map(|condition| PredicateKey::new(PredicateTypeId::AlwaysAccept, condition))
        }

        ssz_proptest!(
            ExecOutputs,
            (
                prop::collection::vec(
                    (any::<[u8; 32]>(), any::<u64>()).prop_map(|(dest, sats)| {
                        OutputTransfer {
                            dest: AccountId::new(dest),
                            value: BitcoinAmount::from_sat(sats),
                        }
                    }),
                    0..10
                ),
                prop::collection::vec(
                    (
                        any::<[u8; 32]>(),
                        any::<u64>(),
                        prop::collection::vec(any::<u8>(), 0..50)
                    )
                        .prop_map(|(dest, sats, data)| {
                            OutputMessage::new(
                                AccountId::new(dest),
                                strata_acct_types::MsgPayload::from_bytes(
                                    BitcoinAmount::from_sat(sats),
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
        fn test_with_new_predicate() {
            let key = PredicateKey::always_accept();
            let outputs = ExecOutputs::new_empty().with_new_predicate(key.clone());
            assert_eq!(outputs.new_predicate(), Some(&key));
        }
    }
}
