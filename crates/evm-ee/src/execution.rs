//! EVM block execution logic.
//!
//! This module provides the core ExecutionEnvironment implementation for EVM blocks,
//! using RSP's sparse state and Reth's EVM execution engine.

use alpen_acct_types::{ExecBlock, ExecBlockOutput, ExecPayload, ExecutionEnvironment};
use alpen_chain_types::{ExecInputs, ExecOutputs, OutputMessage};
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_reth_evm::{
    config::AlpenEvmConfig,
    consensus::{FlooredConsensus, validate_header_extra_against_parent},
    extract_withdrawal_intents,
};
use reth_consensus::{Consensus, FullConsensus, HeaderValidator};
use reth_ethereum_primitives::{Block, BlockBody, Receipt as EthereumReceipt};
use reth_evm::{
    block::BlockExecutionResult,
    execute::{BasicBlockExecutor, BlockExecutionOutput, Executor},
};
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use revm::database::WrapDatabaseRef;
use strata_acct_types::{BRIDGE_GATEWAY_ACCT_ID, BitcoinAmount, MsgPayload};
use strata_codec::encode_to_vec;
use strata_msg_fmt::{Msg as MsgTrait, OwnedMsg};
use strata_ol_msg_types::{DEFAULT_OPERATOR_FEE, WITHDRAWAL_MSG_TYPE_ID, WithdrawalMsgData};

use crate::{
    errors::EvmEnvError,
    types::{EvmBlock, EvmHeader, EvmHeaderIntrinsics, EvmPartialState, EvmWriteBatch},
    utils::{build_and_recover_block, validate_deposits_against_block},
};

/// EVM Execution Environment for Alpen.
///
/// This struct implements the ExecutionEnvironment trait and handles execution
/// of EVM blocks against sparse state using RSP and Reth.
#[derive(Debug, Clone)]
pub struct EvmExecutionEnvironment {
    /// EVM configuration of the one spec version this environment executes.
    evm_config: AlpenEvmConfig,

    /// Consensus rules of that spec version, the same ones the node applies.
    consensus: FlooredConsensus,
}

/// Converts withdrawal intents to messages sent to the bridge gateway account.
///
/// Each withdrawal intent is encoded using `WithdrawalMsgData` containing:
/// - The withdrawal amount (as message value)
/// - The destination descriptor (encoded in message data)
fn convert_withdrawal_intents_to_messages(
    withdrawal_intents: Vec<alpen_reth_primitives::WithdrawalIntent>,
    outputs: &mut ExecOutputs,
) {
    for intent in withdrawal_intents {
        let withdrawal_msg = WithdrawalMsgData::new(
            DEFAULT_OPERATOR_FEE,
            intent.destination.to_bytes().to_vec(),
            intent.selected_operator.raw(),
        )
        .expect("invalid withdrawal destination descriptor");

        let msg_body = encode_to_vec(&withdrawal_msg).expect("encoding failed");
        let msg = OwnedMsg::new(WITHDRAWAL_MSG_TYPE_ID, msg_body).expect("create message");
        let msg_data = msg.to_vec();

        // Create message to bridge gateway with withdrawal amount and encoded data
        let value = BitcoinAmount::try_from(intent.amt)
            .expect("withdrawal amount must be within the bitcoin money supply");
        let payload = MsgPayload::from_bytes(value, msg_data)
            .expect("withdrawal message payload bytes must fit within SSZ max length");
        let message = OutputMessage::new(BRIDGE_GATEWAY_ACCT_ID, payload);
        outputs.add_message(message);
    }
}

/// Checks that the witnessed ancestors belong to the block's chain.
///
/// [`EvmPartialState`] checks that its ancestors are contiguous and link to
/// each other, but that only makes the set self-consistent: a fabricated chain
/// satisfies it just as well as the real one. Pinning the newest ancestor to
/// this block's parent hash anchors the whole set, because the linkage check
/// carries the binding down the rest of it.
fn validate_ancestors_against_block(
    pre_state: &EvmPartialState,
    header_intrinsics: &EvmHeaderIntrinsics,
) -> Result<(), EvmEnvError> {
    let parent_number = header_intrinsics
        .number()
        .checked_sub(1)
        .ok_or(EvmEnvError::NoParent)?;

    // The witness always carries the parent, since a block's ancestor range
    // ends there even when it never uses `BLOCKHASH`.
    let parent_hash = pre_state
        .block_hashes()
        .get(&parent_number)
        .ok_or(EvmEnvError::MissingAncestor(parent_number))?;

    if *parent_hash != header_intrinsics.parent_hash() {
        return Err(EvmEnvError::AncestorMismatch {
            expected: header_intrinsics.parent_hash(),
            witnessed: *parent_hash,
        });
    }

    Ok(())
}

impl EvmExecutionEnvironment {
    /// Creates the environment that executes blocks under `spec_version`, with
    /// the rules `params` gives that version.
    pub fn new(params: &AlpenParams, spec_version: AlpenSpecId) -> Self {
        let evm_config = AlpenEvmConfig::new(params, spec_version);
        let consensus = FlooredConsensus::new(
            params.chain_spec(spec_version).clone(),
            params.fee_spec().base_fee_floor(spec_version),
        );
        Self {
            evm_config,
            consensus,
        }
    }

    fn execute_recovered_block(
        &self,
        block: &RecoveredBlock<Block>,
        pre_state: &EvmPartialState,
    ) -> Result<BlockExecutionOutput<EthereumReceipt>, EvmEnvError> {
        let db = {
            let wit_db = pre_state.create_witness_db();
            WrapDatabaseRef(wit_db)
        };
        // The per-block DA rate (committed in the header `extra_data`) is applied inside
        // `AlpenEvmConfig`'s execution context, which `BasicBlockExecutor` builds per block,
        // so the DA fee charge sees the block's committed rate with no plumbing here.
        let block_executor = BasicBlockExecutor::new(&self.evm_config, db);
        Ok(block_executor.execute(block)?)
    }
}

impl ExecutionEnvironment for EvmExecutionEnvironment {
    type PartialState = EvmPartialState;
    type WriteBatch = EvmWriteBatch;
    type BlockOutput = BlockExecutionResult<EthereumReceipt>;
    type Block = EvmBlock;
    type Error = EvmEnvError;

    fn verify_header(&self, parent: &EvmHeader, header: &EvmHeader) -> Result<(), Self::Error> {
        let header_extra = validate_header_extra_against_parent(header.header(), parent.header())?;

        // The node picks the rules from the header's stamp. This environment
        // holds one version's rules, so it only takes headers stamped with it.
        let expected = self.evm_config.spec_version();
        let got = header_extra.spec_version();
        if got != expected {
            return Err(EvmEnvError::SpecVersionMismatch { expected, got });
        }

        let header = SealedHeader::new_unhashed(header.header().clone());
        let parent = SealedHeader::new_unhashed(parent.header().clone());
        self.consensus.validate_header(&header)?;
        self.consensus
            .validate_header_against_parent(&header, &parent)?;
        Ok(())
    }

    fn verify_block_pre_execution(&self, block: &EvmBlock) -> Result<(), Self::Error> {
        let block = SealedBlock::new_unhashed(Block {
            header: block.get_header().header().clone(),
            body: block.get_body().body().clone(),
        });
        self.consensus.validate_block_pre_execution(&block)?;
        Ok(())
    }

    fn execute_block_body(
        &self,
        pre_state: &Self::PartialState,
        exec_payload: &ExecPayload<'_, Self::Block>,
    ) -> Result<ExecBlockOutput<Self>, Self::Error> {
        // Step 1: Build block from exec_payload and recover senders. The header
        // is checked by `verify_header`, its body by `verify_block_pre_execution`,
        // the inputs by `verify_inputs_against_block` and the execution results
        // by `verify_block_post_execution`.
        let block = build_and_recover_block(exec_payload)?;

        // Step 2: Anchor the `BLOCKHASH` ancestors to this block.
        validate_ancestors_against_block(pre_state, exec_payload.header_intrinsics())?;

        // Step 3: Execute the block.
        let BlockExecutionOutput { result, state } =
            self.execute_recovered_block(&block, pre_state)?;

        // Step 4: Collect withdrawal intents.
        let transactions = block.into_transactions();
        let withdrawal_intents = extract_withdrawal_intents(
            &transactions,
            &result.receipts,
            self.evm_config.bridge_params(),
        )?;

        // Step 5: Convert the state changes to a write batch.
        let write_batch = EvmWriteBatch::from_bundle_state(&state);

        // Step 6: Create ExecOutputs with withdrawal intent messages.
        let mut outputs = ExecOutputs::new_empty();
        convert_withdrawal_intents_to_messages(withdrawal_intents, &mut outputs);

        Ok(ExecBlockOutput::new(write_batch, result, outputs))
    }

    fn verify_inputs_against_block(
        &self,
        block: &Self::Block,
        inputs: &ExecInputs,
    ) -> Result<(), Self::Error> {
        validate_deposits_against_block(block.get_body().body(), inputs)
    }

    fn verify_block_post_execution(
        &self,
        header: &EvmHeader,
        outputs: &ExecBlockOutput<Self>,
    ) -> Result<(), Self::Error> {
        // Reth's rule reads only the header from the block, so the body and
        // senders are left empty rather than copied. If it ever read the body,
        // the empty one would fail valid blocks, not pass invalid ones.
        let block = RecoveredBlock::new_unhashed(
            Block {
                header: header.header().clone(),
                body: BlockBody::default(),
            },
            Vec::new(),
        );
        self.consensus
            .validate_block_post_execution(&block, outputs.block_output(), None)?;
        Ok(())
    }

    fn merge_write_into_state(
        &self,
        state: &mut Self::PartialState,
        wb: &Self::WriteBatch,
    ) -> Result<(), Self::Error> {
        // Merge the HashedPostState into the EthereumState
        state.merge_write_batch(wb);

        Ok(())
    }

    fn update_partial_state_after_block(
        &self,
        state: &mut Self::PartialState,
        header: &<Self::Block as ExecBlock>::Header,
    ) -> Result<(), Self::Error> {
        state.add_executed_block(header.header().clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use alloy_consensus::{
        BlockBody, Header, Sealable, TxLegacy, proofs::calculate_transaction_root,
    };
    use alpen_acct_types::{ExecBlock, ExecHeader, ExecPartialState};
    use alpen_params::{DEV_PARAMS_JSON, HeaderExtra};
    use reth_consensus::ConsensusError;
    use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_primitives_traits::Block as RethBlockTrait;
    use revm::{DatabaseRef, state::Bytecode};
    use revm_primitives::{
        B256,
        alloy_primitives::{Bloom, Signature},
    };
    use rsp_client_executor::io::EthClientExecutorInput;
    use serde::Deserialize;
    use strata_msg_fmt::{Msg, MsgRef};
    use strata_ol_bridge_types::OperatorSelection;
    use strata_ol_msg_types::OLMessageExt;
    use strata_primitives::bitcoin_bosd::Descriptor;

    use super::*;
    use crate::types::{EvmBlock, EvmBlockBody, EvmHeader, EvmPartialState};

    /// The spec version the witness fixture was produced under.
    const FIXTURE_SPEC_VERSION: AlpenSpecId = AlpenSpecId::V0;

    /// The dev network's params, whose genesis the witness fixture was built
    /// on.
    fn dev_params() -> AlpenParams {
        serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse")
    }

    fn rehashed_fixture_bytecodes(bytecodes: Vec<Bytecode>) -> BTreeMap<B256, Bytecode> {
        // The RSP fixture stores bytecodes as a Vec without the original code-hash
        // keys. Re-hashing here preserves the old fixture behavior; production
        // range witnesses pass keyed bytecodes from AccessedStateGenerator.
        bytecodes
            .into_iter()
            .map(|bytecode| (bytecode.hash_slow(), bytecode))
            .collect()
    }

    #[test]
    fn withdrawal_messages_are_sent_to_bridge_gateway_with_msg_envelope() {
        let mut destination_bytes = vec![0x03];
        destination_bytes.extend_from_slice(&[0x22; 20]);
        let destination =
            Descriptor::from_bytes(&destination_bytes).expect("valid p2wpkh descriptor");
        let withdrawal_sats = 1_000_000_000;

        let intent = alpen_reth_primitives::WithdrawalIntent {
            amt: withdrawal_sats,
            selected_operator: OperatorSelection::any(),
            destination,
        };

        let mut outputs = ExecOutputs::new_empty();
        convert_withdrawal_intents_to_messages(vec![intent], &mut outputs);

        let [message] = outputs.output_messages() else {
            panic!("expected exactly one withdrawal output message");
        };
        assert_eq!(message.dest(), BRIDGE_GATEWAY_ACCT_ID);
        assert_eq!(
            message.payload().value(),
            BitcoinAmount::try_from(withdrawal_sats).unwrap()
        );

        let msg = MsgRef::try_from(message.payload().data()).expect("message envelope");
        assert_eq!(msg.ty(), WITHDRAWAL_MSG_TYPE_ID);

        let withdrawal = msg.try_as_withdrawal().expect("withdrawal payload");
        assert_eq!(withdrawal.fees(), DEFAULT_OPERATOR_FEE);
        assert_eq!(
            withdrawal.selected_operator(),
            OperatorSelection::any().raw()
        );
        assert_eq!(withdrawal.dest_desc(), destination_bytes.as_slice());
    }

    #[test]
    fn update_partial_state_after_block_adds_block_hash_for_subsequent_blocks() {
        #[derive(Deserialize, Debug)]
        struct TestData {
            witness: EthClientExecutorInput,
        }

        let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-utils/data/evm-ee/rsp_block_witness.json");

        let json_content = fs::read_to_string(&test_data_path)
            .expect("Failed to read rsp_block_witness.json from test-utils/data/evm-ee");

        let test_data: TestData =
            serde_json::from_str(&json_content).expect("Failed to parse test data");

        let env = EvmExecutionEnvironment::new(&dev_params(), FIXTURE_SPEC_VERSION);
        let header = test_data.witness.current_block.header().clone();
        let evm_header = EvmHeader::new(header.clone());
        let mut state = EvmPartialState::new(
            test_data.witness.parent_state,
            rehashed_fixture_bytecodes(test_data.witness.bytecodes),
            test_data.witness.ancestor_headers,
        );

        // The block has not been executed yet, so its hash is not in the
        // witness. Looking it up fails rather than defaulting to zero.
        assert!(
            state
                .create_witness_db()
                .block_hash_ref(header.number)
                .is_err()
        );

        env.update_partial_state_after_block(&mut state, &evm_header)
            .expect("post-block state update should succeed");

        assert_eq!(
            state
                .create_witness_db()
                .block_hash_ref(header.number)
                .expect("block hash lookup must succeed"),
            header.seal_slow().hash()
        );
    }

    /// A fabricated ancestor chain is internally consistent, so the linkage
    /// check alone accepts it. It must still be rejected, because these hashes
    /// are what `BLOCKHASH` returns.
    #[test]
    fn execute_block_body_rejects_ancestors_from_another_chain() {
        #[derive(Deserialize, Debug)]
        struct TestData {
            witness: EthClientExecutorInput,
        }

        let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-utils/data/evm-ee/rsp_block_witness.json");

        let json_content = fs::read_to_string(&test_data_path)
            .expect("Failed to read rsp_block_witness.json from test-utils/data/evm-ee");

        let test_data: TestData =
            serde_json::from_str(&json_content).expect("Failed to parse test data");

        let env = EvmExecutionEnvironment::new(&dev_params(), FIXTURE_SPEC_VERSION);

        // Same block numbers, different content, relinked so the chain still
        // hangs together on its own terms.
        let mut fabricated: Vec<Header> = Vec::new();
        for (idx, ancestor) in test_data.witness.ancestor_headers.iter().enumerate() {
            let mut header = ancestor.clone();
            header.timestamp += 1;
            if idx > 0 {
                header.parent_hash = fabricated[idx - 1].clone().seal_slow().hash();
            }
            fabricated.push(header);
        }

        let pre_state = EvmPartialState::new(
            test_data.witness.parent_state,
            rehashed_fixture_bytecodes(test_data.witness.bytecodes),
            fabricated,
        );

        let evm_header = EvmHeader::new(test_data.witness.current_block.header().clone());
        let evm_body =
            EvmBlockBody::from_alloy_body(test_data.witness.current_block.body().clone());
        let block = EvmBlock::new(evm_header, evm_body);

        let intrinsics = block.get_header().get_intrinsics();
        let exec_payload = ExecPayload::new(&intrinsics, block.get_body());

        let result = env.execute_block_body(&pre_state, &exec_payload);
        assert!(
            matches!(result, Err(EvmEnvError::AncestorMismatch { .. })),
            "ancestors that do not link to the executed block must be rejected: {result:?}"
        );
    }

    fn fixture_witness() -> EthClientExecutorInput {
        #[derive(Deserialize, Debug)]
        struct TestData {
            witness: EthClientExecutorInput,
        }

        let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-utils/data/evm-ee/rsp_block_witness.json");
        let json_content = fs::read_to_string(test_data_path).expect("read witness fixture");
        let test_data: TestData =
            serde_json::from_str(&json_content).expect("parse witness fixture");
        test_data.witness
    }

    /// The fixture block and its parent, the newest witnessed ancestor.
    ///
    /// The fixture block carries V1's floored base fee but no stamp, which
    /// reads as V0. Stamping it V1 makes it a valid first V1 block over its V0
    /// parent.
    fn fixture_parent_and_header() -> (Header, Header) {
        let witness = fixture_witness();
        let parent = witness
            .ancestor_headers
            .last()
            .expect("witness contains the parent header")
            .clone();
        let mut header = witness.current_block.header().clone();
        header.extra_data = HeaderExtra::new(AlpenSpecId::V1, 0).encode().into();
        (parent, header)
    }

    fn verify_fixture_header(parent: Header, header: Header) -> Result<(), EvmEnvError> {
        let env = EvmExecutionEnvironment::new(&dev_params(), AlpenSpecId::V1);
        env.verify_header(&EvmHeader::new(parent), &EvmHeader::new(header))
    }

    #[test]
    fn verify_header_accepts_the_fixture_block() {
        let (parent, header) = fixture_parent_and_header();

        verify_fixture_header(parent, header).expect("the fixture block follows the header rules");
    }

    /// Each case breaks one rule the node checks against the parent. The
    /// proof must refuse the same headers.
    #[test]
    fn verify_header_rejects_what_the_node_rejects() {
        let (parent, header) = fixture_parent_and_header();

        type BreakRule = fn(&Header, &mut Header);
        type IsExpected = fn(&ConsensusError) -> bool;
        let cases: [(BreakRule, IsExpected); 5] = [
            (
                |_, header| header.parent_hash = B256::ZERO,
                |err| matches!(err, ConsensusError::ParentHashMismatch(_)),
            ),
            (
                |parent, header| header.number = parent.number + 2,
                |err| matches!(err, ConsensusError::ParentBlockNumberMismatch { .. }),
            ),
            (
                |parent, header| header.timestamp = parent.timestamp,
                |err| matches!(err, ConsensusError::TimestampIsInPast { .. }),
            ),
            (
                |parent, header| header.gas_limit = parent.gas_limit * 2,
                |err| matches!(err, ConsensusError::GasLimitInvalidIncrease { .. }),
            ),
            (
                |_, header| header.base_fee_per_gas = header.base_fee_per_gas.map(|fee| fee + 1),
                |err| matches!(err, ConsensusError::BaseFeeDiff(_)),
            ),
        ];

        for (break_rule, is_expected) in cases {
            let mut bad_header = header.clone();
            break_rule(&parent, &mut bad_header);
            let result = verify_fixture_header(parent.clone(), bad_header);
            assert!(
                matches!(&result, Err(EvmEnvError::ConsensusRule(err)) if is_expected(err)),
                "{result:?}"
            );
        }
    }

    /// The node picks a header's rules from its stamp, so a header stamped
    /// with another version must not pass under this environment's rules.
    #[test]
    fn verify_header_rejects_a_header_stamped_with_another_version() {
        let (parent, mut header) = fixture_parent_and_header();
        header.extra_data = HeaderExtra::new(AlpenSpecId::V0, 0).encode().into();

        assert!(matches!(
            verify_fixture_header(parent, header),
            Err(EvmEnvError::SpecVersionMismatch {
                expected: AlpenSpecId::V1,
                got: AlpenSpecId::V0,
            })
        ));
    }

    fn verify_fixture_block(
        header: Header,
        body: BlockBody<TransactionSigned>,
    ) -> Result<(), EvmEnvError> {
        let env = EvmExecutionEnvironment::new(&dev_params(), AlpenSpecId::V1);
        let block = EvmBlock::new(EvmHeader::new(header), EvmBlockBody::from_alloy_body(body));
        env.verify_block_pre_execution(&block)
    }

    #[test]
    fn verify_block_pre_execution_accepts_the_fixture_block() {
        let (_, header) = fixture_parent_and_header();
        let body = fixture_witness().current_block.body().clone();

        verify_fixture_block(header, body).expect("the fixture header commits to its body");
    }

    #[test]
    fn verify_block_pre_execution_rejects_a_body_the_header_does_not_commit_to() {
        let (_, mut header) = fixture_parent_and_header();
        let body = fixture_witness().current_block.body().clone();
        header.transactions_root = B256::ZERO;

        let result = verify_fixture_block(header, body);
        assert!(
            matches!(
                &result,
                Err(EvmEnvError::ConsensusRule(err))
                    if matches!(**err, ConsensusError::BodyTransactionRootDiff(_))
            ),
            "{result:?}"
        );
    }

    /// V1 runs Osaka, so EIP-7934 caps a block's RLP size. The node refuses a
    /// larger block before executing it, and so must the proof.
    #[test]
    fn verify_block_pre_execution_rejects_an_oversized_block() {
        let (_, mut header) = fixture_parent_and_header();
        let mut body = fixture_witness().current_block.body().clone();
        let padding = TxLegacy {
            input: vec![0; MAX_RLP_BLOCK_SIZE].into(),
            ..Default::default()
        };
        body.transactions.push(TransactionSigned::new_unhashed(
            Transaction::Legacy(padding),
            Signature::test_signature(),
        ));
        header.transactions_root = calculate_transaction_root(&body.transactions);

        let result = verify_fixture_block(header, body);
        assert!(
            matches!(
                &result,
                Err(EvmEnvError::ConsensusRule(err))
                    if matches!(**err, ConsensusError::BlockTooLarge { .. })
            ),
            "{result:?}"
        );
    }

    /// Test with real witness data from the reference implementation.
    /// This is an integration test that validates the full execution flow with real block data.
    #[test]
    fn test_with_witness_params() {
        #[derive(Deserialize, Debug)]
        struct TestData {
            witness: EthClientExecutorInput,
        }

        // Load test data from the canonical fixture under test-utils/data/.
        let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-utils/data/evm-ee/rsp_block_witness.json");

        let json_content = fs::read_to_string(&test_data_path)
            .expect("Failed to read rsp_block_witness.json from test-utils/data/evm-ee");

        let test_data: TestData =
            serde_json::from_str(&json_content).expect("Failed to parse test data");

        // Create execution environment
        let env = EvmExecutionEnvironment::new(&dev_params(), FIXTURE_SPEC_VERSION);

        // Use the pre-state directly from witness data (it already has all the proofs!)
        let mut pre_state = EvmPartialState::new(
            test_data.witness.parent_state,
            rehashed_fixture_bytecodes(test_data.witness.bytecodes),
            test_data.witness.ancestor_headers,
        );

        // Create block from witness
        let header = test_data.witness.current_block.header().clone();
        let evm_header = EvmHeader::new(header.clone());

        // Get transactions from the block
        let block_body = test_data.witness.current_block.body().clone();
        let evm_body = EvmBlockBody::from_alloy_body(block_body);

        let block = EvmBlock::new(evm_header, evm_body);

        // Create exec payload and inputs
        let intrinsics = block.get_header().get_intrinsics();
        let exec_payload = ExecPayload::new(&intrinsics, block.get_body());
        let inputs = ExecInputs::new_empty();

        env.verify_inputs_against_block(&block, &inputs)
            .expect("the witness block carries no deposits");

        // Execute the block
        // Note: This will execute real block data through our implementation
        let result = env.execute_block_body(&pre_state, &exec_payload);

        // For now, we just verify it doesn't panic
        // In the future, we can compare outputs with the reference implementation
        assert!(
            result.is_ok(),
            "Block execution should succeed with witness data: {:?}",
            result.err()
        );

        let output = result.expect("block execution should succeed");

        // Check the execution-derived header commitments. This covers the receipts
        // root, logs bloom and gas figures, but not the state root, which this
        // method does not look at.
        env.verify_block_post_execution(block.get_header(), &output)
            .expect("execution-derived commitments should match the witness header");

        // The state root only exists once the writes are merged, so check it here.
        env.merge_write_into_state(&mut pre_state, output.write_batch())
            .expect("merging the write batch should succeed");
        assert_eq!(
            pre_state.compute_state_root().as_ref(),
            header.state_root.as_slice(),
            "merged state root should match the witness header"
        );
    }

    /// Each case breaks one commitment the header makes to execution. The
    /// proof must refuse the same headers the node does.
    #[test]
    fn verify_block_post_execution_rejects_what_the_node_rejects() {
        #[derive(Deserialize, Debug)]
        struct TestData {
            witness: EthClientExecutorInput,
        }

        let test_data_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test-utils/data/evm-ee/rsp_block_witness.json");

        let json_content = fs::read_to_string(&test_data_path)
            .expect("Failed to read rsp_block_witness.json from test-utils/data/evm-ee");

        let test_data: TestData =
            serde_json::from_str(&json_content).expect("Failed to parse test data");

        let env = EvmExecutionEnvironment::new(&dev_params(), FIXTURE_SPEC_VERSION);
        let pre_state = EvmPartialState::new(
            test_data.witness.parent_state,
            rehashed_fixture_bytecodes(test_data.witness.bytecodes),
            test_data.witness.ancestor_headers,
        );

        let header = test_data.witness.current_block.header().clone();
        let evm_header = EvmHeader::new(header.clone());
        let block_body = test_data.witness.current_block.body().clone();
        let evm_body = EvmBlockBody::from_alloy_body(block_body);
        let block = EvmBlock::new(evm_header, evm_body);
        let intrinsics = block.get_header().get_intrinsics();
        let exec_payload = ExecPayload::new(&intrinsics, block.get_body());
        let output = env
            .execute_block_body(&pre_state, &exec_payload)
            .expect("block execution should succeed");

        type BreakRule = fn(&mut Header);
        type IsExpected = fn(&ConsensusError) -> bool;
        let cases: [(BreakRule, IsExpected); 4] = [
            (
                |header| header.receipts_root = B256::from([0x11; 32]),
                |err| matches!(err, ConsensusError::BodyReceiptRootDiff(_)),
            ),
            (
                |header| header.logs_bloom = Bloom::from([0x22; 256]),
                |err| matches!(err, ConsensusError::BodyBloomLogDiff(_)),
            ),
            (
                |header| header.gas_used += 1,
                |err| matches!(err, ConsensusError::BlockGasUsed { .. }),
            ),
            (
                |header| header.requests_hash = Some(B256::from([0x33; 32])),
                |err| matches!(err, ConsensusError::BodyRequestsHashDiff(_)),
            ),
        ];

        for (break_rule, is_expected) in cases {
            let mut bad_header = header.clone();
            break_rule(&mut bad_header);
            let result = env.verify_block_post_execution(&EvmHeader::new(bad_header), &output);
            assert!(
                matches!(&result, Err(EvmEnvError::ConsensusRule(err)) if is_expected(err)),
                "{result:?}"
            );
        }
    }
}
