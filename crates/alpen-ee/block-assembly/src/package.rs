use alpen_ee_common::EnginePayload;
use bitcoin_bosd::Descriptor;
use strata_acct_types::{AccountId, BitcoinAmount, Hash, MsgPayload};
use strata_codec::encode_to_vec;
use strata_ee_acct_types::PendingInputEntry;
use strata_ee_chain_types::{
    ExecBlockCommitment, ExecBlockPackage, ExecInputs, ExecOutputs, OutputMessage,
};
use strata_msg_fmt::{Msg as MsgTrait, OwnedMsg};
use strata_ol_bridge_types::OperatorSelection;
use strata_ol_msg_types::{WithdrawalMsgData, DEFAULT_OPERATOR_FEE, WITHDRAWAL_MSG_TYPE_ID};
use strata_predicate::PredicateKey;
use tracing::{info, warn};

/// Returns the predicate key declared by a consumed `PredicateRotation` entry
/// among `pending_inputs`, if any. Block assembly stops extracting deposits
/// at a rotation entry (never past it), so at most one can appear here.
pub(crate) fn consumed_predicate_rotation(
    pending_inputs: &[PendingInputEntry],
) -> Option<PredicateKey> {
    pending_inputs.iter().find_map(|entry| match entry {
        PendingInputEntry::PredicateRotation(key) => Some(key.clone()),
        PendingInputEntry::Deposit(_) => None,
    })
}

/// Builds [`ExecInputs`] from pending input entries that were executed in the current block.
pub(crate) fn build_block_inputs(pending_inputs: Vec<PendingInputEntry>) -> ExecInputs {
    let mut inputs = ExecInputs::new_empty();
    for pending_input in pending_inputs {
        match pending_input {
            PendingInputEntry::Deposit(subj_deposit_data) => {
                info!(
                    dest_subject = ?subj_deposit_data.dest,
                    amount_sat = subj_deposit_data.value.to_sat(),
                    "accepted deposit message as EE input",
                );
                inputs.add_subject_deposit(subj_deposit_data);
            }
            // Not an EVM input: consuming it is recorded on `ExecOutputs.new_predicate`
            // instead (see `consumed_predicate_rotation`).
            PendingInputEntry::PredicateRotation(_) => {}
        }
    }
    inputs
}

/// Builds [`ExecOutputs`] from withdrawal intents in the payload and any
/// predicate rotation consumed by this block.
pub(crate) fn build_block_outputs<TPayload: EnginePayload>(
    bridge_gateway_account_id: AccountId,
    payload: &TPayload,
    new_predicate: Option<PredicateKey>,
) -> ExecOutputs {
    let mut outputs = ExecOutputs::new_empty();
    outputs.set_new_predicate(new_predicate);
    let withdrawal_intents = payload.withdrawal_intents();
    if !withdrawal_intents.is_empty() {
        info!(
            withdrawal_intent_count = withdrawal_intents.len(),
            "building withdrawal output messages from payload intents",
        );
    }
    for withdrawal_intent in withdrawal_intents {
        let dest_desc_len = withdrawal_intent.destination.to_bytes().len();
        let Some(msg_payload) = create_withdrawal_init_message_payload(
            withdrawal_intent.destination.clone(),
            BitcoinAmount::from_sat(withdrawal_intent.amt),
            withdrawal_intent.selected_operator,
        ) else {
            warn!(
                amount_sat = withdrawal_intent.amt,
                selected_operator = withdrawal_intent.selected_operator.raw(),
                dest_desc_len,
                destination = ?withdrawal_intent.destination,
                "skipping withdrawal: failed to create withdrawal message",
            );
            continue;
        };
        info!(
            amount_sat = withdrawal_intent.amt,
            selected_operator = withdrawal_intent.selected_operator.raw(),
            dest_desc_len,
            destination = ?withdrawal_intent.destination,
            "created withdrawal output message for bridge gateway",
        );
        outputs.add_message(OutputMessage::new(bridge_gateway_account_id, msg_payload));
    }
    outputs
}

/// Builds the block package based on execution inputs and results.
pub(crate) fn build_block_package<TPayload: EnginePayload>(
    bridge_gateway_account_id: AccountId,
    pending_inputs: Vec<PendingInputEntry>,
    payload: &TPayload,
) -> ExecBlockPackage {
    // 1. build block commitment
    let exec_blkid = payload.blockhash();
    // TODO(STR-3682): get using `EvmExecutionEnvironment`
    let raw_block_encoded_hash = Hash::new([0u8; 32]);
    let commitment = ExecBlockCommitment::new(exec_blkid, raw_block_encoded_hash);

    // 2. determine whether this block consumed a predicate rotation, before `build_block_inputs`
    //    consumes `pending_inputs`
    let new_predicate = consumed_predicate_rotation(&pending_inputs);

    // 3. build block inputs
    let inputs = build_block_inputs(pending_inputs);

    // 4. build block outputs
    let outputs = build_block_outputs(bridge_gateway_account_id, payload, new_predicate);

    ExecBlockPackage::new(commitment, inputs, outputs)
}

fn create_withdrawal_init_message_payload(
    dest_desc: Descriptor,
    value: BitcoinAmount,
    selected_operator: OperatorSelection,
) -> Option<MsgPayload> {
    let withdrawal_data = WithdrawalMsgData::new(
        DEFAULT_OPERATOR_FEE,
        dest_desc.to_bytes(),
        selected_operator.raw(),
    )?;
    let body = encode_to_vec(&withdrawal_data).expect("encode withdrawal data");

    let msg = OwnedMsg::new(WITHDRAWAL_MSG_TYPE_ID, body).expect("create message");
    let payload_data = msg.to_vec();

    MsgPayload::from_bytes(value, payload_data).ok()
}

#[cfg(test)]
mod tests {
    use strata_acct_types::SubjectId;
    use strata_ee_chain_types::SubjectDepositData;

    use super::*;

    fn make_deposit(dest_bytes: [u8; 32], sats: u64) -> PendingInputEntry {
        PendingInputEntry::Deposit(SubjectDepositData::new(
            SubjectId::new(dest_bytes),
            BitcoinAmount::from_sat(sats),
        ))
    }

    fn make_rotation() -> PendingInputEntry {
        PendingInputEntry::PredicateRotation(PredicateKey::always_accept())
    }

    #[test]
    fn consumed_predicate_rotation_finds_the_rotation() {
        let entries = vec![make_deposit([0x01; 32], 1000), make_rotation()];
        assert_eq!(
            consumed_predicate_rotation(&entries),
            Some(PredicateKey::always_accept())
        );
    }

    #[test]
    fn consumed_predicate_rotation_none_when_absent() {
        let entries = vec![make_deposit([0x01; 32], 1000)];
        assert_eq!(consumed_predicate_rotation(&entries), None);
    }

    #[test]
    fn build_block_inputs_skips_predicate_rotation() {
        let inputs = vec![make_deposit([0x01; 32], 1000), make_rotation()];

        let block_inputs = build_block_inputs(inputs);

        // Only the deposit becomes an EVM input; the rotation is not one.
        assert_eq!(block_inputs.total_inputs(), 1);
    }

    #[test]
    fn build_block_inputs_converts_pending_deposits() {
        let inputs = vec![
            make_deposit([0x01; 32], 1000),
            make_deposit([0x02; 32], 2000),
            make_deposit([0x03; 32], 3000),
        ];

        let block_inputs = build_block_inputs(inputs);

        assert_eq!(block_inputs.total_inputs(), 3);
        let deposits = block_inputs.subject_deposits();
        assert_eq!(deposits[0].dest(), SubjectId::new([0x01; 32]));
        assert_eq!(deposits[0].value(), BitcoinAmount::from_sat(1000));
        assert_eq!(deposits[1].dest(), SubjectId::new([0x02; 32]));
        assert_eq!(deposits[1].value(), BitcoinAmount::from_sat(2000));
        assert_eq!(deposits[2].dest(), SubjectId::new([0x03; 32]));
        assert_eq!(deposits[2].value(), BitcoinAmount::from_sat(3000));
    }

    #[test]
    fn build_block_inputs_empty() {
        let block_inputs = build_block_inputs(Vec::new());
        assert_eq!(block_inputs.total_inputs(), 0);
    }

    #[test]
    fn build_block_inputs_preserves_deposit_value() {
        let inputs = vec![make_deposit([0xaa; 32], 12345)];

        let block_inputs = build_block_inputs(inputs);

        assert_eq!(block_inputs.total_inputs(), 1);
        assert_eq!(
            block_inputs.subject_deposits()[0].value(),
            BitcoinAmount::from_sat(12345)
        );
    }
}
