use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatus, PayloadStatusEnum};
use alpen_engine_api::{ExecutionEngine, ExecutionEngineError};
use alpen_reth_node::{AlpenBuiltPayload, AlpenEngineTypes};
use async_trait::async_trait;
use reth_node_builder::{BuiltPayload, ConsensusEngineHandle, PayloadTypes};
use strata_retry::retry_with_backoff_async;
use tracing::{debug, info, warn};

use crate::retry::{engine_call_backoff, ENGINE_CALL_MAX_RETRIES};

/// Logs a forkchoice update that reth did not apply.
///
/// SYNCING is routine while a full node follows gossip, so it is only worth more than a debug
/// line when reth also drops the safe and finalized blocks the update carried.
fn log_forkchoice_status(state: &ForkchoiceState, payload_status: &PayloadStatus) {
    let latest_valid_hash = payload_status.latest_valid_hash;
    match &payload_status.status {
        PayloadStatusEnum::Valid => {}
        PayloadStatusEnum::Invalid { validation_error } => {
            warn!(?state, ?latest_valid_hash, %validation_error, "reth rejected forkchoice update");
        }
        PayloadStatusEnum::Syncing
            if !state.safe_block_hash.is_zero() || !state.finalized_block_hash.is_zero() =>
        {
            info!(
                ?state,
                "reth is syncing; dropped safe and finalized from forkchoice update"
            );
        }
        status => debug!(?state, %status, "reth did not apply forkchoice update"),
    }
}

/// Logs a payload that reth did not mark valid.
fn log_new_payload_status(blockhash: B256, blocknum: u64, payload_status: &PayloadStatus) {
    let latest_valid_hash = payload_status.latest_valid_hash;
    match &payload_status.status {
        PayloadStatusEnum::Valid => {}
        PayloadStatusEnum::Invalid { validation_error } => {
            warn!(%blockhash, blocknum, ?latest_valid_hash, %validation_error, "reth rejected payload");
        }
        status => debug!(%blockhash, blocknum, %status, "reth did not validate payload"),
    }
}

/// Execution engine implementation using Reth for Alpen EE.
#[derive(Debug, Clone)]
pub struct AlpenRethExecEngine {
    beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>,
}

impl AlpenRethExecEngine {
    /// Creates a new Alpen Reth execution engine.
    pub fn new(beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>) -> Self {
        Self {
            beacon_engine_handle,
        }
    }
}

#[async_trait]
impl ExecutionEngine for AlpenRethExecEngine {
    type TEnginePayload = AlpenBuiltPayload;

    async fn submit_payload(&self, payload: AlpenBuiltPayload) -> Result<(), ExecutionEngineError> {
        retry_with_backoff_async(
            "exec_engine_submit_payload",
            ENGINE_CALL_MAX_RETRIES,
            &engine_call_backoff(),
            || async {
                let block = payload.block();
                let payload_status = self
                    .beacon_engine_handle
                    .new_payload(AlpenEngineTypes::block_to_payload(block.to_owned()))
                    .await
                    .map_err(|e| ExecutionEngineError::payload_submission(e.to_string()))?;
                log_new_payload_status(block.hash(), block.number, &payload_status);
                Ok(())
            },
        )
        .await
    }

    async fn update_consensus_state(
        &self,
        state: ForkchoiceState,
    ) -> Result<(), ExecutionEngineError> {
        retry_with_backoff_async(
            "exec_engine_update_consensus_state",
            ENGINE_CALL_MAX_RETRIES,
            &engine_call_backoff(),
            || async {
                debug!(?state, "Sending fork choice state to beacon");
                let updated = self
                    .beacon_engine_handle
                    .fork_choice_updated(state, None)
                    .await
                    .map_err(|e| ExecutionEngineError::fork_choice_update(e.to_string()))?;
                log_forkchoice_status(&state, &updated.payload_status);
                Ok(())
            },
        )
        .await
    }
}
