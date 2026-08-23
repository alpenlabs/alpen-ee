use std::num::NonZero;

use alpen_ee_common::{EnginePayload, ExecBlockPayload, PayloadBuilderEngine};
use alpen_ee_params::AlpenSpecId;
use eyre::Context;
use strata_acct_types::{AccountId, Hash, MessageEntry};
use strata_ee_acct_runtime::apply_input_messages;
use strata_ee_acct_types::EeAccountState;
use strata_ee_chain_types::ExecBlockPackage;

use crate::{
    package::build_block_package,
    payload::{build_exec_payload, extract_consumed_inputs, ConsumedInputs},
};

/// All inputs that control the next built block.
#[derive(Debug)]
pub struct BlockAssemblyInputs<'a> {
    /// EeAccountState of last block.
    pub account_state: EeAccountState,
    /// New inbox messages to be included in this block.
    /// Can be empty.
    pub inbox_messages: &'a [MessageEntry],
    /// Exec blkid of previous block.
    pub parent_exec_blkid: Hash,
    /// Timestamp of next block to be built in ms.
    pub timestamp_ms: u64,
    /// Max number of deposits to process per block.
    pub max_deposits_per_block: NonZero<u8>,
    /// Account id for bridge gateway on ol.
    pub bridge_gateway_account_id: AccountId,
    /// Monotonically incrementing index for next deposit to use.
    pub next_deposit_idx: u64,
    /// Alpen spec version governing this block.
    pub spec_version: AlpenSpecId,
}

/// Outputs from block assembly
#[derive(Debug)]
pub struct BlockAssemblyOutputs {
    /// Block package representing the OL inputs and outputs for this block.
    pub package: ExecBlockPackage,
    /// Block payload including full exec block body.
    pub payload: ExecBlockPayload,
    /// EeAccountState after applying the new block.
    pub account_state: EeAccountState,
    /// Monotonically incrementing index for next deposit to use.
    pub next_deposit_idx: u64,
    /// Alpen spec version governing the *next* block. Equal to this block's
    /// own `spec_version` unless this block consumed a queued predicate
    /// rotation, in which case it's that rotation's successor — this block
    /// itself was still built under the predecessor version.
    pub next_spec_version: AlpenSpecId,
}

/// Builds the next block using `inputs` and `payload_builder`.
pub async fn build_next_exec_block<E: PayloadBuilderEngine>(
    inputs: BlockAssemblyInputs<'_>,
    payload_builder: &E,
) -> eyre::Result<BlockAssemblyOutputs> {
    let BlockAssemblyInputs {
        mut account_state,
        inbox_messages,
        parent_exec_blkid,
        timestamp_ms,
        max_deposits_per_block,
        bridge_gateway_account_id,
        next_deposit_idx,
        spec_version,
    } = inputs;

    // 1. apply new inbox messages to account state
    apply_input_messages(&mut account_state, inbox_messages)
        .context("build_next_exec_block: failed to apply input messages")?;

    // 2. work out what this block consumes
    //
    // This is the one place a consumed rotation is determined. Both users of
    // that fact read it from here: the spec-version bump below and the key the
    // package declares in step 5. Deriving it twice would let them disagree.
    let ConsumedInputs {
        deposits,
        processed,
        new_predicate,
    } = extract_consumed_inputs(
        account_state.pending_inputs(),
        max_deposits_per_block,
        next_deposit_idx,
    );

    // 3. resolve the version the *next* block runs under
    //
    // A consumed rotation activates its successor version; this block itself is
    // still built under `spec_version`. If this software doesn't know that
    // successor it has to stop, and it stops here rather than after the build:
    // `build_exec_payload` persists a block witness before it returns, and a
    // block that fails afterwards is never saved. The builder retries on a
    // short backoff while the node waits to be upgraded, and nothing prunes
    // witnesses, so failing late would orphan one per attempt.
    let next_spec_version = if new_predicate.is_some() {
        spec_version
            .successor()
            .map_err(|id| eyre::eyre!("consumed a rotation to unknown spec version {id}"))
            .context("build_next_exec_block: cannot honor discovered spec activation")?
    } else {
        spec_version
    };

    // 4. build exec block payload
    let (payload, update_extra_data, next_deposit_idx) = build_exec_payload(
        deposits,
        processed,
        parent_exec_blkid,
        timestamp_ms,
        next_deposit_idx,
        spec_version,
        payload_builder,
    )
    .await?;

    // 5. update account state based on built payload and consumed inputs
    account_state.set_last_exec_blkid(*update_extra_data.new_tip_blkid());
    account_state.set_last_exec_state_root(*update_extra_data.new_tip_state_root());
    // Drain pending input entries that got executed in the current block.
    let processed_inputs =
        account_state.remove_pending_inputs(*update_extra_data.processed_inputs() as usize);
    let _ = account_state.remove_pending_fincls(*update_extra_data.processed_fincls() as usize);

    // 6. build exec package
    let package = build_block_package(
        bridge_gateway_account_id,
        processed_inputs,
        &payload,
        new_predicate,
    );

    Ok(BlockAssemblyOutputs {
        package,
        payload: ExecBlockPayload::from_bytes(
            payload
                .to_bytes()
                .context("build_next_exec_block: failed to serialized payload")?,
        ),
        account_state,
        next_deposit_idx,
        next_spec_version,
    })
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use alpen_ee_common::{
        ExecutionEngine, ExecutionEngineError, ForkchoiceState, PayloadBuildAttributes,
    };
    use alpen_reth_primitives::WithdrawalIntent;
    use async_trait::async_trait;
    use strata_ee_acct_types::PendingInputEntry;
    use strata_predicate::PredicateKey;

    use super::*;

    #[derive(Clone)]
    struct StubPayload;

    impl EnginePayload for StubPayload {
        type Error = Infallible;

        fn blocknum(&self) -> u64 {
            unreachable!("no payload is ever built in these tests")
        }
        fn blockhash(&self) -> Hash {
            unreachable!("no payload is ever built in these tests")
        }
        fn state_root(&self) -> Hash {
            unreachable!("no payload is ever built in these tests")
        }
        fn withdrawal_intents(&self) -> &[WithdrawalIntent] {
            unreachable!("no payload is ever built in these tests")
        }
        fn to_bytes(&self) -> Result<Vec<u8>, Self::Error> {
            unreachable!("no payload is ever built in these tests")
        }
        fn from_bytes(_bytes: &[u8]) -> Result<Self, Self::Error> {
            unreachable!("no payload is ever built in these tests")
        }
    }

    /// Panics if asked to build a payload.
    ///
    /// Requesting one persists a block witness, so the spec-version check has
    /// to reject the block before we get here.
    struct NeverBuildsEngine;

    #[async_trait]
    impl ExecutionEngine for NeverBuildsEngine {
        type TEnginePayload = StubPayload;

        async fn submit_payload(&self, _p: StubPayload) -> Result<(), ExecutionEngineError> {
            unreachable!("no payload is ever built in these tests")
        }

        async fn update_consensus_state(
            &self,
            _state: ForkchoiceState,
        ) -> Result<(), ExecutionEngineError> {
            unreachable!("no payload is ever built in these tests")
        }
    }

    #[async_trait]
    impl PayloadBuilderEngine for NeverBuildsEngine {
        async fn build_payload(
            &self,
            _attrs: PayloadBuildAttributes,
        ) -> eyre::Result<Self::TEnginePayload> {
            panic!("a payload was requested before the spec version was resolved");
        }
    }

    fn assembly_inputs(
        account_state: EeAccountState,
        spec_version: AlpenSpecId,
    ) -> BlockAssemblyInputs<'static> {
        BlockAssemblyInputs {
            account_state,
            inbox_messages: &[],
            parent_exec_blkid: Hash::zero(),
            timestamp_ms: 1_000_000,
            max_deposits_per_block: NonZero::new(8).unwrap(),
            bridge_gateway_account_id: AccountId::from([0u8; 32]),
            next_deposit_idx: 0,
            spec_version,
        }
    }

    /// A node that doesn't know the version a queued rotation activates must
    /// stop before requesting a payload. Requesting one persists a witness for
    /// a block that is never saved, and since the builder retries on a short
    /// backoff and nothing prunes witnesses, failing later would orphan one per
    /// attempt for as long as the node waits to be upgraded.
    #[tokio::test]
    async fn unknown_successor_fails_before_requesting_a_payload() {
        // V1 is the highest version this binary knows, so its successor is not
        // one it can execute.
        let account_state = EeAccountState::new(
            Hash::zero(),
            Hash::zero(),
            vec![PendingInputEntry::PredicateRotation(
                PredicateKey::always_accept(),
            )],
            vec![],
        );

        let err = build_next_exec_block(
            assembly_inputs(account_state, AlpenSpecId::V1),
            &NeverBuildsEngine,
        )
        .await
        .expect_err("an unknown successor version must stop block building");

        assert!(
            err.to_string().contains("spec activation"),
            "unexpected error: {err}"
        );
    }
}
