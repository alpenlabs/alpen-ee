//! [`Storage`] behaviour of [`NodeStorage`](crate::storage::NodeStorage).

use alpen_acct_types::EeAccountState;
use alpen_storage_api::{OLBlockOrEpoch, Storage, StorageError};
use strata_identifiers::{Buf32, EpochCommitment, OLBlockId};

use super::setup_storage;

fn create_test_block_id(value: u8) -> OLBlockId {
    let mut bytes = [0u8; 32];
    bytes[31] = value;
    // ensure null block is not created
    bytes[0] = 1;
    OLBlockId::from(Buf32::new(bytes))
}

fn create_test_ee_account_state() -> EeAccountState {
    EeAccountState::new([0u8; 32].into(), [0u8; 32].into(), Vec::new(), Vec::new())
}

/// Test storing and retrieving EE account state.
#[tokio::test]
async fn test_store_and_get_ee_account_state() {
    let storage = &setup_storage();
    // Create test data
    let epoch = 1u32;
    let slot = 100u64;
    let block_id = create_test_block_id(1);
    let ol_block = EpochCommitment::new(epoch, slot, block_id);
    let ee_account_state = create_test_ee_account_state();

    // Store the account state
    storage
        .store_ee_account_state(&ol_block, &ee_account_state)
        .await
        .unwrap();

    // Retrieve by block ID
    let retrieved = storage
        .ee_account_state(OLBlockOrEpoch::TerminalBlock(block_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.epoch_commitment(), &ol_block);
    assert_eq!(retrieved.ee_state(), &ee_account_state);

    // Retrieve by slot
    let retrieved_by_epoch = storage
        .ee_account_state(OLBlockOrEpoch::Epoch(epoch))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retrieved_by_epoch.epoch_commitment().last_blkid(),
        &block_id
    );

    // Retrieve best state
    let best = storage.best_ee_account_state().await.unwrap().unwrap();
    assert_eq!(best.epoch_commitment(), &ol_block);
    assert_eq!(best.ee_state(), &ee_account_state);
}

/// Test sequential epoch enforcement.
#[tokio::test]
async fn test_sequential_slots() {
    let storage = &setup_storage();
    // First write should succeed at any epoch
    let epoch1 = 1u32;
    let slot1 = 100u64;
    let block_id1 = create_test_block_id(1);
    let ol_epoch1 = EpochCommitment::new(epoch1, slot1, block_id1);
    let ee_account_state1 = create_test_ee_account_state();

    storage
        .store_ee_account_state(&ol_epoch1, &ee_account_state1)
        .await
        .unwrap();

    // Next write must be at epoch1 + 1
    let epoch2 = epoch1 + 1;
    let slot2 = slot1 + 10;
    let block_id2 = create_test_block_id(2);
    let ol_epoch2 = EpochCommitment::new(epoch2, slot2, block_id2);
    let ee_account_state2 = create_test_ee_account_state();

    storage
        .store_ee_account_state(&ol_epoch2, &ee_account_state2)
        .await
        .unwrap();

    // Writing to a non-sequential epoch should fail
    let epoch_skip = epoch2 + 2; // Skip epoch2 + 1
    let slot_skip = slot2 + 10;
    let block_id_skip = create_test_block_id(3);
    let ol_epoch_skip = EpochCommitment::new(epoch_skip, slot_skip, block_id_skip);
    let ee_account_state_skip = create_test_ee_account_state();

    let result = storage
        .store_ee_account_state(&ol_epoch_skip, &ee_account_state_skip)
        .await;
    assert!(result.is_err());
}

/// Test null epoch rejection.
#[tokio::test]
async fn test_null_block_rejected() {
    let storage = &setup_storage();
    let null_epoch = EpochCommitment::null();
    let ee_account_state = create_test_ee_account_state();

    let result = storage
        .store_ee_account_state(&null_epoch, &ee_account_state)
        .await;
    assert!(result.is_err());
    // The underlying database layer should reject null epochs
}

/// Test rollback functionality.
#[tokio::test]
async fn test_rollback_ee_account_state() {
    let storage = &setup_storage();
    // Create a sequence of states
    let epochs = [1u32, 2, 3, 4, 5];
    let mut block_ids = Vec::new();

    for (i, epoch) in epochs.iter().enumerate() {
        let slot = 100u64 + (i as u64 * 10);
        let block_id = create_test_block_id(*epoch as u8);
        block_ids.push(block_id);
        let ol_epoch = EpochCommitment::new(*epoch, slot, block_id);
        let ee_account_state = create_test_ee_account_state();

        storage
            .store_ee_account_state(&ol_epoch, &ee_account_state)
            .await
            .unwrap();
    }

    // Rollback to epoch 2
    storage.rollback_ee_account_state(2).await.unwrap();

    // Epochs 1 and 2 should still exist
    assert!(storage
        .ee_account_state(OLBlockOrEpoch::Epoch(1))
        .await
        .unwrap()
        .is_some());
    assert!(storage
        .ee_account_state(OLBlockOrEpoch::Epoch(2))
        .await
        .unwrap()
        .is_some());

    // Epochs 3, 4, 5 should be gone (StateNotFound error expected)
    assert!(matches!(
        storage.ee_account_state(OLBlockOrEpoch::Epoch(3)).await,
        Err(StorageError::StateNotFound(3))
    ));
    assert!(matches!(
        storage.ee_account_state(OLBlockOrEpoch::Epoch(4)).await,
        Err(StorageError::StateNotFound(4))
    ));
    assert!(matches!(
        storage.ee_account_state(OLBlockOrEpoch::Epoch(5)).await,
        Err(StorageError::StateNotFound(5))
    ));

    // Best state should be at epoch 2
    let best = storage.best_ee_account_state().await.unwrap().unwrap();
    assert_eq!(best.epoch_commitment().epoch(), 2);
}

/// Test empty storage behavior.
#[tokio::test]
async fn test_empty_storage() {
    let storage = &setup_storage();
    // Best state should be None on empty storage
    let best = storage.best_ee_account_state().await.unwrap();
    assert!(best.is_none());

    // Getting non-existent block ID should return None
    let block_id = create_test_block_id(1);
    let state = storage
        .ee_account_state(OLBlockOrEpoch::TerminalBlock(block_id))
        .await
        .unwrap();
    assert!(state.is_none());

    // Getting non-existent slot should return StateNotFound error
    assert!(matches!(
        storage.ee_account_state(OLBlockOrEpoch::Epoch(999)).await,
        Err(StorageError::StateNotFound(999))
    ));
}

/// Test rollback on empty storage.
#[tokio::test]
async fn test_rollback_empty_storage() {
    let storage = &setup_storage();
    // Rollback on empty storage should succeed (no-op)
    let result = storage.rollback_ee_account_state(100).await;
    assert!(result.is_ok());
}

/// Test multiple sequential writes and retrieval.
#[tokio::test]
async fn test_sequential_writes_and_retrieval() {
    let storage = &setup_storage();
    let num_epochs = 10;
    let start_epoch = 1u32;

    // Write multiple sequential epochs
    for i in 0..num_epochs {
        let epoch = start_epoch + i;
        let slot = 200u64 + (i as u64 * 10);
        let block_id = create_test_block_id(i as u8);
        let ol_epoch = EpochCommitment::new(epoch, slot, block_id);
        let ee_account_state = create_test_ee_account_state();

        storage
            .store_ee_account_state(&ol_epoch, &ee_account_state)
            .await
            .unwrap();
    }

    // Verify all epochs can be retrieved
    for i in 0..num_epochs {
        let epoch = start_epoch + i;
        let expected_slot = 200u64 + (i as u64 * 10);
        let expected_block_id = create_test_block_id(i as u8);

        let state = storage
            .ee_account_state(OLBlockOrEpoch::Epoch(epoch))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.epoch_commitment().epoch(), epoch);
        assert_eq!(state.epoch_commitment().last_slot(), expected_slot);
        assert_eq!(state.epoch_commitment().last_blkid(), &expected_block_id);
    }

    // Best state should be the last one
    let best = storage.best_ee_account_state().await.unwrap().unwrap();
    assert_eq!(
        best.epoch_commitment().epoch(),
        start_epoch + num_epochs - 1
    );
}
