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
    // This is the one place a consumed rotation is determined, so everything
    // that depends on one reads it from here.
    let ConsumedInputs {
        deposits,
        processed,
        new_predicate,
    } = extract_consumed_inputs(
        account_state.pending_inputs(),
        max_deposits_per_block,
        next_deposit_idx,
    );

    // 3. build exec block payload
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

    // 4. update account state based on built payload and consumed inputs
    account_state.set_last_exec_blkid(*update_extra_data.new_tip_blkid());
    account_state.set_last_exec_state_root(*update_extra_data.new_tip_state_root());
    // Drain pending input entries that got executed in the current block.
    let processed_inputs =
        account_state.remove_pending_inputs(*update_extra_data.processed_inputs() as usize);
    let _ = account_state.remove_pending_fincls(*update_extra_data.processed_fincls() as usize);

    // A consumed rotation governs the version for the *next* block, not this
    // one — this block was already built above under `spec_version`. It reads
    // the rotation resolved in step 2, so the version bump and the key the
    // package declares cannot disagree.
    let next_spec_version = if new_predicate.is_some() {
        spec_version
            .successor()
            .map_err(|id| eyre::eyre!("consumed a rotation to unknown spec version {id}"))
            .context("build_next_exec_block: cannot honor discovered spec activation")?
    } else {
        spec_version
    };

    // 5. build exec package
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
