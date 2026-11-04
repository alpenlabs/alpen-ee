use std::{sync::Arc, time::Instant};

use alpen_acct_types::EeAccountState;
use alpen_block_assembly::{build_next_exec_block, BlockAssemblyInputs, BlockAssemblyOutputs};
use alpen_chain_types::ExecBlockPackage;
use alpen_common::{BlockProductionControl, Clock, ExecBlockPayload, ExecBlockRecord, SystemClock};
use alpen_engine_api::{EnginePayload, PayloadBuilderEngine};
use alpen_exec_chain::ExecChainHandle;
use alpen_params::AlpenSpecId;
use alpen_storage_api::ExecBlockStorage;
use eyre::Context;
use metrics::{counter, gauge, histogram};
use strata_acct_types::{Hash, MessageEntry};
use strata_identifiers::{OLBlockCommitment, OLBlockId};
use thiserror::Error;
use tracing::{debug, error, trace, warn};

use crate::{
    block_builder::BlockBuilderConfig,
    metrics::{
        BLOCKS_BUILT, BLOCK_BUILD_FAILURES, BLOCK_BUILD_SECONDS, BLOCK_INBOX_MESSAGES,
        BLOCK_INTERVAL_SECONDS, BLOCK_LATEST_HEIGHT, BLOCK_PAYLOAD_BYTES,
    },
    ol_chain_tracker::OLChainTrackerHandle,
};

/// Error type for block builder that distinguishes retriable from real errors.
#[derive(Debug, Error)]
enum BlockBuilderError {
    /// Timestamp constraint violated - should retry immediately without backoff.
    #[error("blocktime constraint violated")]
    BlocktimeConstraintViolated,
    /// [`BlockProductionControl`] declined this turn - skip it.
    #[error("block production stopped")]
    ProductionStopped,
    /// Real error occurred - should backoff before retry.
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

/// Computes the target timestamp for the next block.
fn compute_next_block_target(
    last_block: &ExecBlockRecord,
    config: &BlockBuilderConfig,
) -> BlockTarget {
    BlockTarget {
        parent: last_block.blockhash(),
        timestamp_ms: last_block.timestamp_ms() + config.blocktime_ms(),
    }
}

/// Validates that current time meets the minimum blocktime constraint.
fn validate_blocktime_constraint(
    current_timestamp_ms: u64,
    last_block_timestamp_ms: u64,
    blocktime_ms: u64,
) -> Result<(), BlockBuilderError> {
    let min_timestamp = last_block_timestamp_ms + blocktime_ms;
    if current_timestamp_ms < min_timestamp {
        return Err(BlockBuilderError::BlocktimeConstraintViolated);
    }
    Ok(())
}

/// Determines whether inbox messages should be fetched based on OL block state.
fn should_fetch_inbox_messages(last_local_ol_blkid: &OLBlockId, best_ol_blkid: &OLBlockId) -> bool {
    last_local_ol_blkid != best_ol_blkid
}

/// Picks the OL block commitment to record on the next exec block.
///
/// During restart catch-up, finalization lag, or reorg handling, the OL
/// tracker can report a block behind the local EE tip. Returning
/// `last_local_ol_block` in that case keeps the OL slot recorded on the EE
/// chain monotonic; using the older `best_ol_block` would cause inbox
/// fetching to re-include slots already reflected in account state.
fn effective_ol_block(
    best_ol_block: OLBlockCommitment,
    last_local_ol_block: OLBlockCommitment,
) -> OLBlockCommitment {
    if best_ol_block.slot() < last_local_ol_block.slot() {
        last_local_ol_block
    } else {
        best_ol_block
    }
}

/// Computes the inbox-fetch slot range for the next exec block.
///
/// Returns `None` when no fetch is needed: either the OL tip is unchanged
/// from what's already in account state, or the slot delta is empty.
/// Otherwise returns `Some((from_slot, to_slot))` with both ends inclusive.
///
/// `from_slot = last_local_ol_slot + 1` is the off-by-one fix: the previously
/// recorded slot's messages are already drained, so re-fetching it produces a
/// SAU whose `processed_messages` does not match `new_next_inbox_msg_idx` and
/// OL rejects it with "invalid next msg index".
fn compute_inbox_fetch_range(
    last_local_ol_slot: u64,
    last_local_ol_blkid: &OLBlockId,
    effective_ol_slot: u64,
    effective_ol_blkid: &OLBlockId,
) -> Option<(u64, u64)> {
    if !should_fetch_inbox_messages(last_local_ol_blkid, effective_ol_blkid) {
        return None;
    }
    let from_slot = last_local_ol_slot.saturating_add(1);
    if from_slot > effective_ol_slot {
        return None;
    }
    Some((from_slot, effective_ol_slot))
}

/// Constructs BlockAssemblyInputs from the current state.
fn create_block_assembly_inputs<'a>(
    last_local_block: &ExecBlockRecord,
    inbox_messages: &'a [MessageEntry],
    timestamp_ms: u64,
    config: &BlockBuilderConfig,
    spec_version: AlpenSpecId,
) -> BlockAssemblyInputs<'a> {
    BlockAssemblyInputs {
        account_state: last_local_block.account_state().clone(),
        inbox_messages,
        parent_exec_blkid: last_local_block.package().exec_blkid(),
        timestamp_ms,
        max_deposits_per_block: config.max_deposits_per_block(),
        bridge_gateway_account_id: config.bridge_gateway_account_id(),
        next_deposit_idx: last_local_block.next_deposit_idx(),
        spec_version,
    }
}

/// Creates an ExecBlockRecord from block assembly outputs.
#[expect(clippy::too_many_arguments, reason = "too many args")]
fn create_next_exec_block_record(
    package: ExecBlockPackage,
    account_state: EeAccountState,
    last_blocknum: u64,
    best_ol_block: OLBlockCommitment,
    timestamp_ms: u64,
    parent_blockhash: Hash,
    next_inbox_msg_idx: u64,
    next_deposit_idx: u64,
    next_spec_version: AlpenSpecId,
    messages: Vec<MessageEntry>,
) -> ExecBlockRecord {
    ExecBlockRecord::new(
        package,
        account_state,
        last_blocknum + 1,
        best_ol_block,
        timestamp_ms,
        parent_blockhash,
        next_inbox_msg_idx,
        next_deposit_idx,
        next_spec_version,
        messages,
    )
}

pub async fn block_builder_task<
    TPayloadBuilder: PayloadBuilderEngine,
    TStorage: ExecBlockStorage,
    TControl: BlockProductionControl,
>(
    config: BlockBuilderConfig,
    exec_chain_handle: ExecChainHandle,
    ol_chain_handle: OLChainTrackerHandle,
    payload_builder: Arc<TPayloadBuilder>,
    storage: Arc<TStorage>,
    block_production: Arc<TControl>,
) {
    let last_local_block = exec_chain_handle
        .get_best_block()
        .await
        .expect("next_block_target_timestamp: failed to get best exec block");
    let last_hash = last_local_block.parent_blockhash();
    debug!(%last_hash, "last local block parent");

    let mut next_block_target = compute_next_block_target(&last_local_block, &config);
    debug!(?next_block_target, "next block target");

    let clock = SystemClock;
    loop {
        match block_builder_task_inner(
            &next_block_target,
            &config,
            &exec_chain_handle,
            &ol_chain_handle,
            payload_builder.as_ref(),
            storage.as_ref(),
            block_production.as_ref(),
            &clock,
        )
        .await
        {
            Ok((blockhash, next_target)) => {
                debug!(?blockhash, "built new block");
                counter!(BLOCKS_BUILT).increment(1);
                next_block_target = next_target;
            }
            Err(BlockBuilderError::BlocktimeConstraintViolated) => {
                warn!("blocktime constraint violated, retrying immediately");
                counter!(BLOCK_BUILD_FAILURES, "reason" => "blocktime_violation").increment(1);
            }
            Err(BlockBuilderError::ProductionStopped) => {
                trace!("block production stopped, skipping turn");
                clock.sleep_ms(config.blocktime_ms()).await;
            }
            Err(BlockBuilderError::Other(err)) => {
                error!(?err, "failed to build block");
                counter!(BLOCK_BUILD_FAILURES, "reason" => "error").increment(1);
                clock.sleep_ms(config.error_backoff_ms()).await;
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "threads the builder's dependencies"
)]
async fn block_builder_task_inner<TEngine: PayloadBuilderEngine>(
    next_block_target: &BlockTarget,
    config: &BlockBuilderConfig,
    exec_chain_handle: &ExecChainHandle,
    ol_chain_handle: &OLChainTrackerHandle,
    payload_builder: &TEngine,
    storage: &impl ExecBlockStorage,
    block_production: &impl BlockProductionControl,
    clock: &impl Clock,
) -> Result<(Hash, BlockTarget), BlockBuilderError> {
    // if we are not ready, sleep
    clock.sleep_until(next_block_target.timestamp_ms).await;
    let started = Instant::now();

    // we can build blocks now
    let BuiltBlock {
        block,
        payload,
        blockhash,
        parent_timestamp_ms,
    } = build_next_block(
        next_block_target,
        config,
        exec_chain_handle,
        ol_chain_handle,
        payload_builder,
        block_production,
        clock,
    )
    .await?;
    histogram!(BLOCK_BUILD_SECONDS, "stage" => "assemble").record(started.elapsed().as_secs_f64());

    // encode the payload for storage first, since submitting it takes it
    let encoded_payload = payload
        .to_bytes()
        .context("block_builder: serialize payload")?;
    histogram!(BLOCK_PAYLOAD_BYTES).record(encoded_payload.len() as f64);
    let payload_bytes = ExecBlockPayload::from_bytes(encoded_payload);

    // submit the built payload back to engine so reth knows the block (inserts
    // it into the engine tree as VALID, not yet canonical)
    payload_builder
        .submit_payload(payload)
        .await
        .context("block_builder: submit payload to engine")?;

    // cache what the metrics and the next target need before the save consumes the block
    let next_block_target = compute_next_block_target(&block, config);
    let blocknum = block.blocknum();
    let inbox_message_count = block.messages().len();
    let block_timestamp_ms = block.timestamp_ms();

    // save block outputs to Alpen storage (the source of truth)
    storage
        .save_exec_block(block, payload_bytes)
        .await
        .context("block_builder: save exec block to storage")?;

    // submit block to chain tracker
    exec_chain_handle
        .new_block(blockhash)
        .await
        .context("block_builder: submit new exec block")?;
    gauge!(BLOCK_LATEST_HEIGHT, "status" => "head").set(blocknum as f64);
    counter!(BLOCK_INBOX_MESSAGES).increment(inbox_message_count as u64);
    histogram!(BLOCK_INTERVAL_SECONDS)
        .record(block_timestamp_ms.saturating_sub(parent_timestamp_ms) as f64 / 1000.0);
    histogram!(BLOCK_BUILD_SECONDS, "stage" => "total").record(started.elapsed().as_secs_f64());

    // Canonicalization (forkchoice update) is intentionally NOT performed here.
    // The engine control task is the single owner of reth's canonical head: the
    // `new_block` submission above propagates the new tip through
    // exec-chain -> preconf -> engine control, which drives the forkchoice
    // update. Driving it here as well raced that task's cached head and could
    // make reth silently drop OL safe/finalized updates (a behind-canonical
    // head FCU is a no-op for safe/finalized in reth).

    // TODO(STR-3682): should this wait for block

    Ok((blockhash, next_block_target))
}

// Next Block building target data
#[derive(Debug)]
struct BlockTarget {
    parent: Hash,
    timestamp_ms: u64,
}

/// A block built but not yet submitted, with what the caller records once it is.
struct BuiltBlock<P> {
    block: ExecBlockRecord,
    payload: P,
    blockhash: Hash,
    /// Timestamp of the parent, for the block interval.
    parent_timestamp_ms: u64,
}

async fn build_next_block<TEngine: PayloadBuilderEngine>(
    expected_block_target: &BlockTarget,
    config: &BlockBuilderConfig,
    exec_chain_handle: &ExecChainHandle,
    ol_chain_handle: &OLChainTrackerHandle,
    payload_builder: &TEngine,
    block_production: &impl BlockProductionControl,
    clock: &impl Clock,
) -> Result<BuiltBlock<TEngine::TEnginePayload>, BlockBuilderError> {
    let last_local_block = exec_chain_handle
        .get_best_block()
        .await
        .context("build_next_block: failed to get best exec block")?;

    // Checked after the blocktime wait so a change made during it applies to
    // this turn. A block already past this point still completes.
    if !block_production.should_build_next_block(&last_local_block) {
        return Err(BlockBuilderError::ProductionStopped);
    }

    // Check the parent expected by the block target. A mismatch means another
    // builder advanced the local EE tip.
    if last_local_block.blockhash() != expected_block_target.parent {
        warn!(
            expected = %expected_block_target.parent,
            actual = %last_local_block.blockhash(),
            "build_next_block: unexpected latest blockhash"
        )
    }

    // Enforce the configured blocktime against the current local tip.
    let timestamp_ms = clock.current_timestamp();
    validate_blocktime_constraint(
        timestamp_ms,
        last_local_block.timestamp_ms(),
        config.blocktime_ms(),
    )?;

    // check if there are new OL block inputs that need to be included
    let best_ol_block = ol_chain_handle
        .get_finalized_block()
        .await
        .context("build_next_block: failed to get finalized OL block")?;

    let effective_ol_block = effective_ol_block(best_ol_block, *last_local_block.ol_block());

    let (inbox_messages, next_inbox_msg_idx) = match compute_inbox_fetch_range(
        last_local_block.ol_block().slot(),
        last_local_block.ol_block().blkid(),
        effective_ol_block.slot(),
        effective_ol_block.blkid(),
    ) {
        Some((from_slot, to_slot)) => ol_chain_handle
            .get_inbox_messages(from_slot, to_slot)
            .await
            .context("build_next_block: failed to get inbox messages")?
            .into_parts(),
        None => (vec![], last_local_block.next_inbox_msg_idx()),
    };

    // The version governing this block: whatever the parent block's account
    // state already settled on. A rotation consumed by *this* block only
    // takes effect for the block built after it (see `next_spec_version` on
    // `BlockAssemblyOutputs`).
    let spec_version = last_local_block.next_spec_version();

    // build next block
    let block_assembly_inputs = create_block_assembly_inputs(
        &last_local_block,
        &inbox_messages,
        timestamp_ms,
        config,
        spec_version,
    );

    let BlockAssemblyOutputs {
        package,
        payload,
        account_state,
        next_deposit_idx,
        next_spec_version,
    } = build_next_exec_block(block_assembly_inputs, payload_builder)
        .await
        .context("build_next_block: failed to build exec block")?;

    let blockhash = package.exec_blkid();
    let parent_blockhash = last_local_block.package().exec_blkid();
    let block = create_next_exec_block_record(
        package,
        account_state,
        last_local_block.blocknum(),
        effective_ol_block,
        timestamp_ms,
        parent_blockhash,
        next_inbox_msg_idx,
        next_deposit_idx,
        next_spec_version,
        inbox_messages,
    );

    Ok(BuiltBlock {
        block,
        payload,
        blockhash,
        parent_timestamp_ms: last_local_block.timestamp_ms(),
    })
}

#[cfg(test)]
mod tests {
    use std::vec;

    use alpen_chain_types::{ExecBlockCommitment, ExecInputs, ExecOutputs};
    use strata_acct_types::BitcoinAmount;
    use strata_identifiers::Buf32;

    use super::*;

    /// Helper to create an OLBlockCommitment with a given slot.
    fn make_ol_block(slot: u64) -> OLBlockCommitment {
        let mut blkid_bytes = [0u8; 32];
        blkid_bytes[0..8].copy_from_slice(&slot.to_le_bytes());
        OLBlockCommitment::new(slot, OLBlockId::from(Buf32::from(blkid_bytes)))
    }

    /// Helper to create a test ExecBlockRecord.
    fn make_exec_block_record(
        blocknum: u64,
        timestamp_ms: u64,
        ol_block: OLBlockCommitment,
    ) -> ExecBlockRecord {
        let hash = Hash::from(Buf32::new([blocknum as u8; 32]));
        let package = ExecBlockPackage::new(
            ExecBlockCommitment::new(hash, hash),
            ExecInputs::new_empty(),
            ExecOutputs::new_empty(),
        );
        let account_state = EeAccountState::new(hash, Hash::zero(), vec![], vec![]);
        ExecBlockRecord::new(
            package,
            account_state,
            blocknum,
            ol_block,
            timestamp_ms,
            Hash::default(),
            0,
            0,
            AlpenSpecId::V0,
            vec![],
        )
    }

    mod validate_blocktime_constraint_tests {
        use super::*;

        #[test]
        fn boundary_exactly_at_min_timestamp_succeeds() {
            // Edge case: current time equals exactly last_block + blocktime
            // This is the minimum valid time - should pass
            let result = validate_blocktime_constraint(2000, 1000, 1000);
            assert!(result.is_ok());
        }

        #[test]
        fn fails_when_clock_appears_to_go_backwards() {
            // Defensive: if current time is before last block (clock drift/reset),
            // should fail the constraint check
            let result = validate_blocktime_constraint(500, 1000, 1000);
            assert!(matches!(
                result,
                Err(BlockBuilderError::BlocktimeConstraintViolated)
            ));
        }
    }

    mod create_block_assembly_inputs_tests {
        use strata_acct_types::{AccountId, MsgPayload};

        use super::*;

        #[test]
        fn preserves_message_order() {
            // Message order matters for deterministic block assembly
            let ol_block = make_ol_block(10);
            let exec_record = make_exec_block_record(5, 5000, ol_block);
            let config = BlockBuilderConfig::default();

            let msg1 = MessageEntry::new(
                AccountId::new([1u8; 32]),
                0,
                MsgPayload::from_bytes(BitcoinAmount::try_from(100).unwrap(), vec![])
                    .expect("message payload bytes must fit within SSZ max length"),
            );
            let msg2 = MessageEntry::new(
                AccountId::new([2u8; 32]),
                0,
                MsgPayload::from_bytes(BitcoinAmount::try_from(200).unwrap(), vec![])
                    .expect("message payload bytes must fit within SSZ max length"),
            );
            let messages = vec![msg1.clone(), msg2.clone()];

            let inputs = create_block_assembly_inputs(
                &exec_record,
                &messages,
                6000,
                &config,
                AlpenSpecId::V0,
            );

            assert_eq!(inputs.inbox_messages.len(), 2);
            assert_eq!(inputs.inbox_messages[0].source(), msg1.source());
            assert_eq!(inputs.inbox_messages[1].source(), msg2.source());
        }
    }

    mod effective_ol_block_tests {
        use super::*;

        #[test]
        fn picks_best_when_best_is_ahead() {
            let last = make_ol_block(5);
            let best = make_ol_block(10);
            assert_eq!(effective_ol_block(best, last), best);
        }

        #[test]
        fn picks_best_when_slots_equal() {
            // Equal slots: best wins. Different blkid at the same slot still
            // means the OL tracker advanced (e.g. reorg sibling).
            let last = make_ol_block(5);
            let best = make_ol_block(5);
            assert_eq!(effective_ol_block(best, last), best);
        }

        #[test]
        fn falls_back_to_last_when_best_is_behind() {
            // Restart catch-up / finalization lag: tracker behind the local
            // tip. Falling back to `last` keeps the recorded OL slot
            // monotonic; using `best` here would re-include drained slots.
            let last = make_ol_block(10);
            let best = make_ol_block(5);
            assert_eq!(effective_ol_block(best, last), last);
        }
    }

    mod compute_inbox_fetch_range_tests {
        use super::*;

        fn blkid(byte: u8) -> OLBlockId {
            let mut bytes = [0u8; 32];
            bytes[31] = byte;
            OLBlockId::from(Buf32::from(bytes))
        }

        #[test]
        fn returns_none_when_blkids_match() {
            // OL tip unchanged: nothing new to fetch even if slots happened
            // to differ (would mean a slot was rewritten in place).
            let id = blkid(1);
            assert_eq!(compute_inbox_fetch_range(5, &id, 5, &id), None);
        }

        #[test]
        fn returns_none_when_range_is_empty() {
            // Different blkids but `from > to` — can happen if `effective_ol`
            // falls back to `last_local` after a behind-tracker case and the
            // siblings carry the same slot but different ids. No slots to
            // pull.
            assert_eq!(compute_inbox_fetch_range(5, &blkid(1), 5, &blkid(2)), None);
        }

        #[test]
        fn fetches_only_new_slots_off_by_one_regression() {
            // Last local OL block at slot 5, new effective at slot 7.
            // The fetch must start at slot 6, NOT slot 5: re-including slot 5
            // would re-process messages already drained into account state
            // and OL would reject the resulting SAU with "invalid next msg
            // index".
            assert_eq!(
                compute_inbox_fetch_range(5, &blkid(1), 7, &blkid(2)),
                Some((6, 7)),
            );
        }

        #[test]
        fn single_slot_advance_returns_singleton_range() {
            // Slot 5 -> slot 6: fetch [6, 6].
            assert_eq!(
                compute_inbox_fetch_range(5, &blkid(1), 6, &blkid(2)),
                Some((6, 6)),
            );
        }
    }
}
