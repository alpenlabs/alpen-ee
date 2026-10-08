use alpen_common::{ExecBlockRecord, ExecBlockStorage, SequencerOLClient};
use eyre::eyre;
use strata_identifiers::OLBlockCommitment;
use tracing::error;

use super::fetch::{fetch_blocks_after, Extension};
use crate::OLChainTrackerState;

/// Initializes tracker state by syncing from local storage and the OL client.
///
/// The tracker starts from the OL block of `exec_tip` when the OL has finalized that
/// block, and otherwise from the OL block of the last finalized exec block. Starting at
/// the tip keeps the tracker from asking the OL again for blocks the EE has already
/// consumed. An OL node promoted from checkpoint sync can't serve blocks at or below its
/// history anchor.
pub async fn init_ol_chain_tracker_state<TStorage: ExecBlockStorage, TClient: SequencerOLClient>(
    storage: &TStorage,
    exec_tip: &ExecBlockRecord,
    ol_client: &TClient,
) -> eyre::Result<OLChainTrackerState> {
    // last finalized block known to EE sequencer locally
    let finalized_exec_block = storage
        .best_finalized_block()
        .await?
        .ok_or(eyre!("finalized block missing"))?;
    let local_finalized_ol_block = *finalized_exec_block.ol_block();

    // chain status according to OL
    // TODO(STR-3682): retry
    let ol_chain_status = ol_client.chain_status().await?;
    let remote_finalized_ol_block = ol_chain_status.finalized().to_block_commitment();

    if remote_finalized_ol_block.slot() < local_finalized_ol_block.slot() {
        // Block height that is considered finalized locally is not considered finalized on OL.
        //
        // Either a deep reorg has occurred on OL,
        // or a significant mismatch between OL and EE.
        // In either case, exit to avoid corrupting local data and await manual resolution.
        error!(
            local = ?local_finalized_ol_block,
            remote = ?remote_finalized_ol_block,
            "local finalized OL block ahead of OL"
        );
        return Err(eyre!(
            "local finalized state is ahead of connected OL's finalized slot"
        ));
    }

    // In `latest` epoch tracking mode the tip can use an OL block that the OL hasn't
    // finalized yet. The finalized exec block's OL block is then the newest safe start.
    //
    // Starting at the tip leaves out the OL blocks between the finalized exec block's OL
    // block and the tip's. Only a rollback of the exec chain below its tip would need them.
    let start_block = if exec_tip.ol_block().slot() <= remote_finalized_ol_block.slot() {
        exec_tip
    } else {
        &finalized_exec_block
    };
    let start_ol_block = *start_block.ol_block();

    // The exec block records the OL's inbox index at its OL block, so the OL
    // doesn't have to serve that block again.
    let mut state =
        OLChainTrackerState::new_empty(start_ol_block, start_block.next_inbox_msg_idx());

    if remote_finalized_ol_block.slot() == start_ol_block.slot() {
        if remote_finalized_ol_block != start_ol_block {
            return Err(deep_reorg_error(start_ol_block, remote_finalized_ol_block));
        }
        return Ok(state);
    }

    // TODO(STR-3682): retry
    // TODO(STR-3682): chunk calls by slot range
    let extension =
        fetch_blocks_after(ol_client, start_ol_block, remote_finalized_ol_block.slot()).await?;

    let blocks = match extension {
        Extension::Blocks(blocks) => blocks,
        Extension::Diverged(remote_block) => {
            return Err(deep_reorg_error(start_ol_block, remote_block));
        }
    };

    // Everything looks ok now. Build local state.
    for block in blocks {
        state.append_block(
            block.commitment,
            block.inbox_messages,
            block.next_inbox_msg_idx,
        )?;
    }

    Ok(state)
}

/// Logs and builds the error for a local finalized OL block that the OL chain doesn't
/// contain.
///
/// The OL chain has seen a deep reorg. Exiting avoids corrupting local data while
/// waiting for manual resolution.
fn deep_reorg_error(local: OLBlockCommitment, remote: OLBlockCommitment) -> eyre::Report {
    error!(
        ?local,
        ?remote,
        "local finalized OL block not present in OL"
    );
    eyre!("local finalized state not present in OL chain. Deep reorg detected.")
}

#[cfg(test)]
mod tests {
    use super::*;

    mod init_ol_chain_tracker_state_tests {
        use alpen_common::{
            MockExecBlockStorage, MockSequencerOLClient, OLChainStatus, OLClientError,
        };

        use super::*;
        use crate::ol_chain_tracker::test_utils::{
            create_block_data_chain, create_mock_exec_record,
            create_mock_exec_record_with_inbox_idx, make_block_link, make_block_with_id,
            make_chain_status,
        };

        // =========================================================================
        // Test Helpers
        // =========================================================================

        /// Sets up mock storage to return the given exec record as best finalized block.
        fn setup_mock_storage_finalized(
            mock_storage: &mut MockExecBlockStorage,
            exec_record: alpen_common::ExecBlockRecord,
        ) {
            mock_storage
                .expect_best_finalized_block()
                .times(1)
                .returning(move || Ok(Some(exec_record.clone())));
        }

        /// Sets up mock OL client to return the given chain status.
        fn setup_mock_client_chain_status(
            mock_client: &mut MockSequencerOLClient,
            status: OLChainStatus,
        ) {
            mock_client
                .expect_chain_status()
                .times(1)
                .returning(move || Ok(status));
        }

        /// Sets up mock OL client to return inbox messages for the given block data.
        fn setup_mock_client_inbox_messages(
            mock_client: &mut MockSequencerOLClient,
            block_data: Vec<alpen_common::OLBlockData>,
        ) {
            mock_client
                .expect_get_inbox_messages()
                .times(1)
                .returning(move |_, _| Ok(block_data.clone()));
        }

        /// Sets up mock OL client to return the given block link.
        fn setup_mock_client_block_link(
            mock_client: &mut MockSequencerOLClient,
            link: alpen_common::OLBlockLink,
        ) {
            mock_client
                .expect_get_block_link()
                .withf(move |slot| *slot == link.commitment.slot())
                .times(1)
                .returning(move |_| Ok(link));
        }

        // =========================================================================
        // Tests
        // =========================================================================

        #[tokio::test]
        async fn returns_empty_state_when_synced() {
            // Scenario: Local and remote are at the same finalized block
            //
            // Local chain:   [...] -> [slot=10, id=10] (finalized)
            // Remote chain:  [...] -> [slot=10, id=10] (finalized)
            //
            // Expected: Empty state with base at slot 10, no blocks fetched

            let finalized_block = make_block_with_id(10, 10);
            let exec_record = create_mock_exec_record(finalized_block);
            let chain_status = make_chain_status(finalized_block);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);

            let state = init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client)
                .await
                .unwrap();

            assert_eq!(state.best_block(), finalized_block);
            assert!(state.blocks().is_empty());
        }

        #[tokio::test]
        async fn takes_base_inbox_idx_from_local_record() {
            // Scenario: Local and remote are synced; the local exec record says the
            // OL inbox index at its OL block is 7.
            //
            // Expected: The tracker's base index is 7, without asking the OL.

            let finalized_block = make_block_with_id(10, 10);
            let exec_record = create_mock_exec_record_with_inbox_idx(finalized_block, 7);
            let chain_status = make_chain_status(finalized_block);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);

            let state = init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client)
                .await
                .unwrap();

            let messages = state.get_inbox_messages(11, 11).unwrap();
            assert!(messages.messages().is_empty());
            assert_eq!(messages.next_inbox_msg_idx(), 7);
        }

        #[tokio::test]
        async fn builds_state_from_new_blocks() {
            // Scenario: Remote is ahead of local by 3 blocks
            //
            // Local chain:   [...] -> [slot=10, id=10] (finalized)
            // Remote chain:  [...] -> [slot=10, id=10] -> [slot=11] -> [slot=12] -> [slot=13]
            // (finalized)
            //
            // Expected: State with base at slot 10, blocks 11-13 tracked

            let local_finalized = make_block_with_id(10, 10);
            let remote_finalized = make_block_with_id(13, 13);

            // Blocks after the local finalized block
            let ol_blocks: Vec<_> = (11..=13)
                .map(|slot| make_block_with_id(slot, slot as u8))
                .collect();
            let block_data = create_block_data_chain(&ol_blocks, 0);

            let exec_record = create_mock_exec_record(local_finalized);
            let chain_status = make_chain_status(remote_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);
            setup_mock_client_block_link(
                &mut mock_client,
                make_block_link(ol_blocks[0], local_finalized),
            );
            setup_mock_client_inbox_messages(&mut mock_client, block_data);

            let state = init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client)
                .await
                .unwrap();

            // Base should be local finalized (slot 10)
            assert_eq!(state.base().slot(), 10);
            // Should have 3 new blocks tracked (11, 12, 13)
            assert_eq!(state.blocks().len(), 3);
            assert_eq!(state.best_block().slot(), 13);

            // Verify messages were stored
            let messages = state.get_inbox_messages(11, 13).unwrap();
            assert_eq!(messages.messages().len(), 3);
        }

        #[tokio::test]
        async fn starts_from_exec_tip_when_ol_finalized_it() {
            // Scenario: The exec tip used a newer OL block than the finalized exec block,
            // and the OL has finalized past it.
            //
            // Finalized exec block -> OL [slot=5]
            // Exec tip             -> OL [slot=10], inbox index 4
            // Remote chain:  [...] -> [slot=10, id=10] -> [slot=11] -> [slot=12] (finalized)
            //
            // Expected: Base at slot 10 with index 4. Slots up to 10 are not fetched.

            let finalized_record = create_mock_exec_record(make_block_with_id(5, 5));
            let tip_ol_block = make_block_with_id(10, 10);
            let exec_tip = create_mock_exec_record_with_inbox_idx(tip_ol_block, 4);
            let remote_finalized = make_block_with_id(12, 12);

            let ol_blocks: Vec<_> = (11..=12)
                .map(|slot| make_block_with_id(slot, slot as u8))
                .collect();
            let block_data = create_block_data_chain(&ol_blocks, 4);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, finalized_record);
            setup_mock_client_chain_status(&mut mock_client, make_chain_status(remote_finalized));
            setup_mock_client_block_link(
                &mut mock_client,
                make_block_link(ol_blocks[0], tip_ol_block),
            );
            mock_client
                .expect_get_inbox_messages()
                .withf(|min, max| *min == 11 && *max == 12)
                .times(1)
                .returning(move |_, _| Ok(block_data.clone()));

            let state = init_ol_chain_tracker_state(&mock_storage, &exec_tip, &mock_client)
                .await
                .unwrap();

            assert_eq!(*state.base(), tip_ol_block);
            assert_eq!(state.best_block(), ol_blocks[1]);
        }

        #[tokio::test]
        async fn starts_from_finalized_block_when_tip_ahead_of_ol() {
            // Scenario: The exec tip used an OL block the OL hasn't finalized yet, as in
            // `latest` epoch tracking mode.
            //
            // Finalized exec block -> OL [slot=10, id=10]
            // Exec tip             -> OL [slot=15]
            // Remote chain:  [...] -> [slot=10, id=10] -> [slot=11] (finalized)
            //
            // Expected: Base at slot 10, block 11 tracked

            let local_finalized = make_block_with_id(10, 10);
            let finalized_record = create_mock_exec_record(local_finalized);
            let exec_tip = create_mock_exec_record(make_block_with_id(15, 15));
            let remote_finalized = make_block_with_id(11, 11);
            let block_data = create_block_data_chain(&[remote_finalized], 0);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, finalized_record);
            setup_mock_client_chain_status(&mut mock_client, make_chain_status(remote_finalized));
            setup_mock_client_block_link(
                &mut mock_client,
                make_block_link(remote_finalized, local_finalized),
            );
            setup_mock_client_inbox_messages(&mut mock_client, block_data);

            let state = init_ol_chain_tracker_state(&mock_storage, &exec_tip, &mock_client)
                .await
                .unwrap();

            assert_eq!(*state.base(), local_finalized);
            assert_eq!(state.best_block(), remote_finalized);
        }

        #[tokio::test]
        async fn errors_when_finalized_block_missing() {
            // Scenario: Storage has no finalized block
            //
            // Local chain:   (empty)
            // Remote chain:  [...] -> [slot=10] (finalized)
            //
            // Expected: Error "finalized block missing"

            let exec_record = create_mock_exec_record(make_block_with_id(10, 10));

            let mut mock_storage = MockExecBlockStorage::new();
            let mock_client = MockSequencerOLClient::new();

            mock_storage
                .expect_best_finalized_block()
                .returning(|| Ok(None));

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("finalized block missing"));
        }

        #[tokio::test]
        async fn errors_when_local_ahead_of_remote() {
            // Scenario: Local finalized slot is ahead of remote finalized slot
            //
            // Local chain:   [...] -> [slot=15, id=15] (finalized)
            // Remote chain:  [...] -> [slot=10, id=10] (finalized)
            //
            // Expected: Error about local being ahead

            let local_finalized = make_block_with_id(15, 15);
            let remote_finalized = make_block_with_id(10, 10);

            let exec_record = create_mock_exec_record(local_finalized);
            let chain_status = make_chain_status(remote_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("local finalized state is ahead"));
        }

        #[tokio::test]
        async fn errors_on_deep_reorg() {
            // Scenario: Remote's next block builds on a different block (deep reorg)
            //
            // Local chain:   [...] -> [slot=10, id=0xAA] (finalized)
            // Remote chain:  [...] -> [slot=10, id=0xBB] -> [slot=11] (finalized)
            //                         ^ different block at same slot!
            //
            // Expected: Error "Deep reorg detected"

            let local_finalized = make_block_with_id(10, 0xAA);
            let remote_finalized = make_block_with_id(11, 11);
            let remote_block_at_10 = make_block_with_id(10, 0xBB);

            let exec_record = create_mock_exec_record(local_finalized);
            let chain_status = make_chain_status(remote_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);
            setup_mock_client_block_link(
                &mut mock_client,
                make_block_link(remote_finalized, remote_block_at_10),
            );

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Deep reorg detected"));
        }

        #[tokio::test]
        async fn errors_on_same_slot_different_block() {
            // Scenario: Remote finalized block is at the local slot but differs
            //
            // Local chain:   [...] -> [slot=10, id=0xAA] (finalized)
            // Remote chain:  [...] -> [slot=10, id=0xBB] (finalized)
            //
            // Expected: Error "Deep reorg detected"

            let local_finalized = make_block_with_id(10, 0xAA);
            let remote_finalized = make_block_with_id(10, 0xBB);

            let exec_record = create_mock_exec_record(local_finalized);
            let chain_status = make_chain_status(remote_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Deep reorg detected"));
        }

        #[tokio::test]
        async fn errors_when_chain_status_fails() {
            // Scenario: OL client fails to return chain status
            //
            // Expected: Error propagated from client

            let local_finalized = make_block_with_id(10, 10);
            let exec_record = create_mock_exec_record(local_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            mock_client
                .expect_chain_status()
                .returning(|| Err(OLClientError::network("connection refused")));

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("connection refused"));
        }

        #[tokio::test]
        async fn errors_when_get_inbox_messages_fails() {
            // Scenario: OL client fails to return inbox messages
            //
            // Local chain:   [...] -> [slot=10, id=10] (finalized)
            // Remote chain:  [...] -> [slot=10] -> [slot=11] (finalized)
            //
            // Expected: Error propagated from client

            let local_finalized = make_block_with_id(10, 10);
            let remote_finalized = make_block_with_id(11, 11);

            let exec_record = create_mock_exec_record(local_finalized);
            let chain_status = make_chain_status(remote_finalized);

            let mut mock_storage = MockExecBlockStorage::new();
            let mut mock_client = MockSequencerOLClient::new();

            setup_mock_storage_finalized(&mut mock_storage, exec_record.clone());
            setup_mock_client_chain_status(&mut mock_client, chain_status);
            setup_mock_client_block_link(
                &mut mock_client,
                make_block_link(remote_finalized, local_finalized),
            );
            mock_client
                .expect_get_inbox_messages()
                .returning(|_, _| Err(OLClientError::network("timeout fetching messages")));

            let result =
                init_ol_chain_tracker_state(&mock_storage, &exec_record, &mock_client).await;

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("timeout fetching messages"));
        }
    }
}
