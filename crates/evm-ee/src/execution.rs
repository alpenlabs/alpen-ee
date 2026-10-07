//! EVM block execution logic.
//!
//! This module provides the core ExecutionEnvironment implementation for EVM blocks,
//! using RSP's sparse state and Reth's EVM execution engine.

use alloy_consensus::Header;
use alpen_acct_types::{ExecBlockOutput, ExecutionEnvironment};
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
use reth_primitives_traits::RecoveredBlock;
use revm::database::WrapDatabaseRef;
use strata_acct_types::{BRIDGE_GATEWAY_ACCT_ID, BitcoinAmount, MsgPayload};
use strata_codec::encode_to_vec;
use strata_msg_fmt::{Msg as MsgTrait, OwnedMsg};
use strata_ol_msg_types::{DEFAULT_OPERATOR_FEE, WITHDRAWAL_MSG_TYPE_ID, WithdrawalMsgData};

use crate::{
    errors::EvmEnvError,
    types::{EvmBlock, EvmHeader, EvmPartialState, EvmWriteBatch},
    utils::{recover_block, validate_deposits_against_block},
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
    header: &Header,
) -> Result<(), EvmEnvError> {
    let parent_number = header.number.checked_sub(1).ok_or(EvmEnvError::NoParent)?;

    // The witness always carries the parent, since a block's ancestor range
    // ends there even when it never uses `BLOCKHASH`.
    let parent_hash = pre_state
        .block_hashes()
        .get(&parent_number)
        .ok_or(EvmEnvError::MissingAncestor(parent_number))?;

    if *parent_hash != header.parent_hash {
        return Err(EvmEnvError::AncestorMismatch {
            expected: header.parent_hash,
            witnessed: *parent_hash,
        });
    }

    Ok(())
}

impl EvmExecutionEnvironment {
    /// Creates the environment that executes blocks under `spec_version`, with
    /// the rules `params` gives that version.
    ///
    /// It rejects any block whose `extra_data` stamp names another version.
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

        self.consensus.validate_header(header.sealed())?;
        self.consensus
            .validate_header_against_parent(header.sealed(), parent.sealed())?;
        Ok(())
    }

    fn verify_block_pre_execution(
        &self,
        block: &EvmBlock,
        claimed_inputs: &ExecInputs,
    ) -> Result<(), Self::Error> {
        self.consensus
            .validate_block_pre_execution(block.sealed())?;
        validate_deposits_against_block(block.body(), claimed_inputs)
    }

    fn execute_block(
        &self,
        pre_state: &Self::PartialState,
        block: EvmBlock,
    ) -> Result<ExecBlockOutput<Self>, Self::Error> {
        // Step 1: Recover the senders. The header is checked by
        // `verify_header`, the body and inputs by `verify_block_pre_execution`
        // and the execution results by `verify_block_post_execution`.
        let recovered = recover_block(block)?;

        // Step 2: Anchor the `BLOCKHASH` ancestors to this block.
        validate_ancestors_against_block(pre_state, recovered.header())?;

        // Step 3: Execute the block.
        let BlockExecutionOutput { result, state } =
            self.execute_recovered_block(&recovered, pre_state)?;

        // Step 4: Collect withdrawal intents.
        let withdrawal_intents = extract_withdrawal_intents(
            &recovered.body().transactions,
            &result.receipts,
            self.evm_config.bridge_params(),
        )?;

        // Step 5: Convert the state changes to a write batch.
        let write_batch = EvmWriteBatch::from_bundle_state(
            &state,
            recovered.into_sealed_block().into_sealed_header(),
        );

        // Step 6: Create ExecOutputs with withdrawal intent messages.
        let mut outputs = ExecOutputs::new_empty();
        convert_withdrawal_intents_to_messages(withdrawal_intents, &mut outputs);

        Ok(ExecBlockOutput::new(write_batch, result, outputs))
    }

    fn verify_block_post_execution(
        &self,
        header: &EvmHeader,
        exec_output: &ExecBlockOutput<Self>,
        claimed_outputs: &ExecOutputs,
    ) -> Result<(), Self::Error> {
        // Reth's rule reads only the header from the block, so the body and
        // senders are left empty rather than copied. If it ever read the body,
        // the empty one would fail valid blocks, not pass invalid ones.
        // TODO(STR-4597): the rule takes an owned block, so the header is copied here.
        // Having the trait hand the header over by value would avoid that
        // copy, but `process_block` keeps the header for the state root check
        // and as the next block's parent, so it needs the header back.
        let block = RecoveredBlock::new_unhashed(
            Block {
                header: header.header().clone(),
                body: BlockBody::default(),
            },
            Vec::new(),
        );
        self.consensus
            .validate_block_post_execution(&block, exec_output.block_output(), None)?;

        if !claimed_outputs.matches_executed(exec_output.outputs()) {
            return Err(EvmEnvError::OutputMismatch {
                claimed: Box::new(claimed_outputs.clone()),
                executed: Box::new(exec_output.outputs().clone()),
            });
        }
        Ok(())
    }

    fn merge_write_into_state(
        &self,
        state: &mut Self::PartialState,
        wb: &Self::WriteBatch,
    ) -> Result<(), Self::Error> {
        state.merge_write_batch(wb);
        // Record the block so the next one in the chunk can build on it.
        state.add_executed_block(wb.header().clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        BlockBody, Header, Sealable, TxLegacy, proofs::calculate_transaction_root,
    };
    use alloy_rlp::decode_exact;
    use alpen_acct_types::{ExecBlock, ExecPartialState};
    use alpen_chain_types::{OutputTransfer, SubjectDepositData};
    use alpen_params::DEV_PARAMS_JSON;
    use alpen_test_utils_evm_workload::{MIXED_WORKLOAD, Workload, workload_dir};
    use reth_consensus::ConsensusError;
    use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_primitives_traits::SealedHeader;
    use reth_trie::HashedPostState;
    use revm::DatabaseRef;
    use revm_primitives::{
        B256,
        alloy_primitives::{Bloom, Signature},
    };
    use strata_acct_types::{AccountId, SubjectId};
    use strata_codec::decode_buf_exact;
    use strata_msg_fmt::{Msg, MsgRef};
    use strata_ol_bridge_types::OperatorSelection;
    use strata_ol_msg_types::OLMessageExt;
    use strata_primitives::bitcoin_bosd::Descriptor;

    use super::*;
    use crate::types::{EvmBlock, EvmHeader, EvmPartialState};

    /// The spec version the workload's blocks are stamped with.
    const FIXTURE_SPEC_VERSION: AlpenSpecId = AlpenSpecId::V1;

    /// The dev network's params, whose genesis the workload was built on.
    fn dev_params() -> AlpenParams {
        serde_json::from_str(DEV_PARAMS_JSON).expect("dev params should parse")
    }

    fn fixture_env() -> EvmExecutionEnvironment {
        EvmExecutionEnvironment::new(&dev_params(), FIXTURE_SPEC_VERSION)
    }

    /// The first block of the mixed workload and what it runs on.
    struct Fixture {
        parent: Header,
        block: Block,
        inputs: ExecInputs,
        /// State at the start of the workload. It covers every block of the
        /// range, so it is enough for the first.
        pre_state: EvmPartialState,
    }

    impl Fixture {
        fn evm_block(&self) -> EvmBlock {
            EvmBlock::new(self.block.clone())
        }
    }

    fn load_workload() -> Workload {
        Workload::load(&workload_dir(MIXED_WORKLOAD)).expect("load workload")
    }

    fn fixture() -> Fixture {
        let workload = load_workload();
        let first = workload.blocks.first().expect("workload has blocks");

        let mut inputs = ExecInputs::new_empty();
        for deposit in &first.deposits {
            inputs.add_subject_deposit(SubjectDepositData::new(
                SubjectId::from(deposit.dest_subject),
                BitcoinAmount::try_from(deposit.sats).expect("deposit amount fits"),
            ));
        }

        Fixture {
            parent: decode_exact(&workload.prev_header_rlp[..]).expect("decode prev header"),
            block: decode_exact(&first.block_rlp[..]).expect("decode block"),
            inputs,
            pre_state: decode_buf_exact(&workload.chunk_pre_state).expect("decode pre-state"),
        }
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
    fn merge_write_into_state_adds_block_hash_for_subsequent_blocks() {
        let workload = load_workload();
        let mut pre_state: EvmPartialState =
            decode_buf_exact(&workload.chunk_pre_state).expect("decode pre-state");
        // The pre-state already holds the hashes the range's own blocks read,
        // so take the last block, whose hash no block reads.
        let last = workload.blocks.last().expect("workload has blocks");
        let header = decode_exact::<Block>(&last.block_rlp[..])
            .expect("decode block")
            .header;

        // The block has not been executed yet, so its hash is not in the
        // witness. Looking it up fails rather than defaulting to zero.
        assert!(
            pre_state
                .create_witness_db()
                .block_hash_ref(header.number)
                .is_err()
        );

        let write_batch = EvmWriteBatch::new(
            HashedPostState::default(),
            SealedHeader::new_unhashed(header.clone()),
        );
        fixture_env()
            .merge_write_into_state(&mut pre_state, &write_batch)
            .expect("merging the write batch should succeed");

        assert_eq!(
            pre_state
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
    fn execute_block_rejects_ancestors_from_another_chain() {
        let fixture = fixture();

        // Same block numbers, different content, relinked so the chain still
        // hangs together on its own terms.
        let mut fabricated: Vec<Header> = Vec::new();
        for (idx, ancestor) in fixture.pre_state.ancestor_headers().values().enumerate() {
            let mut header = ancestor.inner().clone();
            header.timestamp += 1;
            if idx > 0 {
                header.parent_hash = fabricated[idx - 1].clone().seal_slow().hash();
            }
            fabricated.push(header);
        }

        let pre_state = EvmPartialState::new(
            fixture.pre_state.ethereum_state().clone(),
            fixture.pre_state.bytecodes().clone(),
            fabricated,
        );

        let result = fixture_env().execute_block(&pre_state, fixture.evm_block());
        assert!(
            matches!(result, Err(EvmEnvError::AncestorMismatch { .. })),
            "ancestors that do not link to the executed block must be rejected: {result:?}"
        );
    }

    fn verify_fixture_header(parent: Header, header: Header) -> Result<(), EvmEnvError> {
        fixture_env().verify_header(&EvmHeader::new(parent), &EvmHeader::new(header))
    }

    #[test]
    fn verify_header_accepts_the_fixture_block() {
        let Fixture { parent, block, .. } = fixture();

        verify_fixture_header(parent, block.header)
            .expect("the fixture block follows the header rules");
    }

    /// Each case breaks one rule the node checks against the parent. The
    /// proof must refuse the same headers.
    #[test]
    fn verify_header_rejects_what_the_node_rejects() {
        let Fixture { parent, block, .. } = fixture();
        let header = block.header;
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
        let Fixture { parent, block, .. } = fixture();
        let env = EvmExecutionEnvironment::new(&dev_params(), AlpenSpecId::V0);

        assert!(matches!(
            env.verify_header(&EvmHeader::new(parent), &EvmHeader::new(block.header)),
            Err(EvmEnvError::SpecVersionMismatch {
                expected: AlpenSpecId::V0,
                got: AlpenSpecId::V1,
            })
        ));
    }

    fn verify_fixture_block(
        header: Header,
        body: BlockBody<TransactionSigned>,
        inputs: &ExecInputs,
    ) -> Result<(), EvmEnvError> {
        let block = EvmBlock::new(Block { header, body });
        fixture_env().verify_block_pre_execution(&block, inputs)
    }

    #[test]
    fn verify_block_pre_execution_accepts_the_fixture_block() {
        let Fixture { block, inputs, .. } = fixture();

        verify_fixture_block(block.header, block.body, &inputs)
            .expect("the fixture header commits to its body");
    }

    #[test]
    fn verify_block_pre_execution_rejects_a_body_the_header_does_not_commit_to() {
        let Fixture { block, inputs, .. } = fixture();
        let mut header = block.header;
        header.transactions_root = B256::ZERO;

        let result = verify_fixture_block(header, block.body, &inputs);
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
        let Fixture { block, inputs, .. } = fixture();
        let mut header = block.header;
        let mut body = block.body;
        let padding = TxLegacy {
            input: vec![0; MAX_RLP_BLOCK_SIZE].into(),
            ..Default::default()
        };
        body.transactions.push(TransactionSigned::new_unhashed(
            Transaction::Legacy(padding),
            Signature::test_signature(),
        ));
        header.transactions_root = calculate_transaction_root(&body.transactions);

        let result = verify_fixture_block(header, body, &inputs);
        assert!(
            matches!(
                &result,
                Err(EvmEnvError::ConsensusRule(err))
                    if matches!(**err, ConsensusError::BlockTooLarge { .. })
            ),
            "{result:?}"
        );
    }

    /// The proof guests build the environment for one version. A block stamped
    /// for another version passes the checks on the block body, so execution
    /// itself has to reject it, not only `verify_header`.
    #[test]
    fn execute_block_rejects_a_block_stamped_for_another_version() {
        let fixture = fixture();
        let env = EvmExecutionEnvironment::new(&dev_params(), AlpenSpecId::V0);
        let block = fixture.evm_block();

        env.verify_block_pre_execution(&block, &fixture.inputs)
            .expect("the body checks do not look at the stamp");
        let result = env.execute_block(&fixture.pre_state, block);
        assert!(
            matches!(result, Err(EvmEnvError::Execution(_))),
            "a V1-stamped block must not execute under V0: {result:?}"
        );
    }

    /// Runs a real block through every step the chunk proof takes and checks
    /// the result against the header the node built.
    #[test]
    fn execute_block_matches_the_node_built_header() {
        let mut fixture = fixture();
        let env = fixture_env();
        let block = fixture.evm_block();
        let header = block.get_header().clone();

        env.verify_block_pre_execution(&block, &fixture.inputs)
            .expect("the header commits to the body, which mints the workload's deposits");

        let output = env
            .execute_block(&fixture.pre_state, block)
            .expect("block execution should succeed");

        // This covers the receipts root, logs bloom and gas figures, but not
        // the state root, which this method does not look at.
        env.verify_block_post_execution(&header, &output, output.outputs())
            .expect("execution-derived commitments should match the header");

        // The state root only exists once the writes are merged, so check it here.
        env.merge_write_into_state(&mut fixture.pre_state, output.write_batch())
            .expect("merging the write batch should succeed");
        assert_eq!(
            fixture.pre_state.compute_state_root().as_ref(),
            fixture.block.header.state_root.as_slice(),
            "merged state root should match the header"
        );
    }

    #[test]
    fn verify_block_post_execution_rejects_mismatched_outputs() {
        let fixture = fixture();
        let env = fixture_env();
        let block = fixture.evm_block();
        let header = block.get_header().clone();
        let output = env
            .execute_block(&fixture.pre_state, block)
            .expect("block execution should succeed");

        let mut claimed = output.outputs().clone();
        claimed.add_transfer(OutputTransfer::new(
            AccountId::new([0x44; 32]),
            BitcoinAmount::try_from(1u64).expect("amount fits"),
        ));

        let result = env.verify_block_post_execution(&header, &output, &claimed);
        assert!(
            matches!(result, Err(EvmEnvError::OutputMismatch { .. })),
            "{result:?}"
        );
    }

    /// Each case breaks one commitment the header makes to execution. The
    /// proof must refuse the same headers the node does.
    #[test]
    fn verify_block_post_execution_rejects_what_the_node_rejects() {
        let fixture = fixture();
        let env = fixture_env();
        let output = env
            .execute_block(&fixture.pre_state, fixture.evm_block())
            .expect("block execution should succeed");
        let header = fixture.block.header;

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
            let result = env.verify_block_post_execution(
                &EvmHeader::new(bad_header),
                &output,
                output.outputs(),
            );
            assert!(
                matches!(&result, Err(EvmEnvError::ConsensusRule(err)) if is_expected(err)),
                "{result:?}"
            );
        }
    }
}
