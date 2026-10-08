//! MDBX-backed implementation of [`RecoveredDaDatabase`].

use std::{ops::ControlFlow, path::Path, sync::Arc};

use alpen_da_l1_extraction::{DaL1Ref, RecoveredDaBlob};
use alpen_da_types::decode_da_blob;
use alpen_mdbx::{Direction, KeyCodec, MdbxConfig, MdbxEnv, ValueCodec};
use alpen_params::AlpenSpecId;
use bitcoin::{hashes::Hash as _, Txid};
use strata_identifiers::L1Height;

use super::schema::{recovered_da_tables, RecoveredDaSchema};
use crate::{
    database::RecoveredDaDatabase,
    error::{RecoveredDaDbError, RecoveredDaDbResult},
    serialization_types::{DBRecoveredDaBlob, RecoveredDaKey},
};

/// MDBX-backed recovered EE DA database over a dedicated [`MdbxEnv`].
#[derive(Debug)]
pub(crate) struct RecoveredDaDbMdbx {
    env: Arc<MdbxEnv>,
}

impl RecoveredDaDbMdbx {
    /// Opens a standalone environment at `path` with only recovered DA tables.
    pub(crate) fn open(path: &Path, config: &MdbxConfig) -> RecoveredDaDbResult<Self> {
        let env = MdbxEnv::open(path, config, &recovered_da_tables())?;
        Ok(Self { env: Arc::new(env) })
    }
}

impl RecoveredDaDatabase for RecoveredDaDbMdbx {
    fn put(&self, recovered_blobs: Vec<RecoveredDaBlob>) -> RecoveredDaDbResult<()> {
        let stored_blobs = recovered_blobs
            .into_iter()
            .map(encode_recovered_blob)
            .collect::<RecoveredDaDbResult<Vec<_>>>()?;

        self.env.update(|writer| {
            for (key, stored_blob) in &stored_blobs {
                match writer.get::<RecoveredDaSchema>(key)? {
                    Some(existing) if existing != *stored_blob => {
                        return Err(RecoveredDaDbError::BlobConflict {
                            update_seq_no: key.update_seq_no(),
                            commit_txid: key.commit_txid(),
                        });
                    }
                    Some(_) => {}
                    None => writer.put::<RecoveredDaSchema>(key, stored_blob)?,
                }
            }
            Ok(())
        })
    }

    fn get_contiguous_from(
        &self,
        first_update_seq_no: u64,
        recovered_l1_frontier: L1Height,
    ) -> RecoveredDaDbResult<Vec<RecoveredDaBlob>> {
        let first_key = RecoveredDaKey::new(first_update_seq_no, Txid::all_zeros());
        let first_key_bytes = first_key.encode_key().map_err(alpen_mdbx::DbError::from)?;
        let stored_blobs = self.env.view(|reader| {
            let mut blobs = Vec::new();
            let mut expected_update_seq_no = first_update_seq_no;
            let mut found_eligible_candidate = false;
            let upgrade_ctx = reader.upgrade_ctx();
            reader.walk::<RecoveredDaSchema>(
                Some(&first_key_bytes),
                Direction::Forward,
                |key_bytes, value_bytes| {
                    let key = RecoveredDaKey::decode_key(key_bytes)?;
                    if key.update_seq_no() > expected_update_seq_no {
                        if !found_eligible_candidate {
                            return Ok(ControlFlow::Break(()));
                        }
                        let Some(next_expected_update_seq_no) =
                            expected_update_seq_no.checked_add(1)
                        else {
                            return Ok(ControlFlow::Break(()));
                        };
                        expected_update_seq_no = next_expected_update_seq_no;
                        found_eligible_candidate = false;
                        if key.update_seq_no() != expected_update_seq_no {
                            return Ok(ControlFlow::Break(()));
                        }
                    }

                    let blob = DBRecoveredDaBlob::decode_value(value_bytes, &upgrade_ctx)?;
                    if blob.completion_block().height() <= recovered_l1_frontier {
                        found_eligible_candidate = true;
                        blobs.push((key, blob));
                    }
                    Ok(ControlFlow::Continue(()))
                },
            )?;
            Ok::<_, RecoveredDaDbError>(blobs)
        })?;

        stored_blobs
            .into_iter()
            .map(|(key, blob)| decode_recovered_blob(key, blob))
            .collect()
    }

    fn prune_before(&self, update_seq_no: u64) -> RecoveredDaDbResult<()> {
        self.env.update(|writer| {
            let mut pruned_keys = Vec::new();
            writer.for_each::<RecoveredDaSchema>(|key, _| {
                if key.update_seq_no() < update_seq_no {
                    pruned_keys.push(key);
                }
                Ok(())
            })?;

            for key in pruned_keys {
                writer.delete::<RecoveredDaSchema>(&key)?;
            }
            Ok(())
        })
    }

    fn clear(&self) -> RecoveredDaDbResult<()> {
        self.env
            .update(|writer| writer.clear::<RecoveredDaSchema>())?;
        Ok(())
    }
}

fn encode_recovered_blob(
    recovered_blob: RecoveredDaBlob,
) -> RecoveredDaDbResult<(RecoveredDaKey, DBRecoveredDaBlob)> {
    let update_seq_no = recovered_blob.blob().update_seq_no;
    let commit_txid = recovered_blob.l1_ref().commit_txid();
    let completion_block = recovered_blob.l1_ref().completion_block();
    let spec_version = recovered_blob.blob().spec_version;
    let payload_bytes = recovered_blob.blob().encode_to_vec()?;

    Ok((
        RecoveredDaKey::new(update_seq_no, commit_txid),
        DBRecoveredDaBlob::new(completion_block, spec_version, payload_bytes),
    ))
}

fn decode_recovered_blob(
    key: RecoveredDaKey,
    stored_blob: DBRecoveredDaBlob,
) -> RecoveredDaDbResult<RecoveredDaBlob> {
    let (completion_block, stored_spec_version, payload_bytes) = stored_blob.into_parts();
    let spec_version = AlpenSpecId::try_from(stored_spec_version)
        .map_err(|_| RecoveredDaDbError::UnsupportedStoredSpecVersion(stored_spec_version))?;
    let blob = decode_da_blob(&payload_bytes, spec_version)?;
    let actual = blob.update_seq_no;
    let expected_update_seq_no = key.update_seq_no();
    if actual != expected_update_seq_no {
        return Err(RecoveredDaDbError::StoredUpdateSeqNoMismatch {
            expected: expected_update_seq_no,
            actual,
        });
    }

    let l1_ref = DaL1Ref::new(key.commit_txid(), completion_block);
    Ok(RecoveredDaBlob::new(l1_ref, blob))
}

#[cfg(test)]
mod tests {
    use alpen_da_types::{DaBlob, EvmHeaderSummary};
    use alpen_params::HeaderExtra;
    use bitcoin::Txid;
    use strata_identifiers::{Buf32, L1BlockCommitment, L1BlockId};
    use tempfile::TempDir;

    use super::*;

    fn setup_db() -> (TempDir, RecoveredDaDbMdbx) {
        let datadir = tempfile::tempdir().expect("create temporary recovered DA database");
        let database = RecoveredDaDbMdbx::open(datadir.path(), &MdbxConfig::small())
            .expect("open recovered DA database");
        (datadir, database)
    }

    fn make_block(height: u32, seed: u8) -> L1BlockCommitment {
        L1BlockCommitment::new(height, L1BlockId::from(Buf32::from([seed; 32])))
    }

    fn build_blob(
        update_seq_no: u64,
        block_num: u64,
        txid_seed: u8,
        completion_block: L1BlockCommitment,
    ) -> RecoveredDaBlob {
        build_blob_for_spec(
            AlpenSpecId::V1,
            update_seq_no,
            block_num,
            txid_seed,
            completion_block,
        )
    }

    fn build_blob_for_spec(
        spec_version: AlpenSpecId,
        update_seq_no: u64,
        block_num: u64,
        txid_seed: u8,
        completion_block: L1BlockCommitment,
    ) -> RecoveredDaBlob {
        let l1_ref = DaL1Ref::new(Txid::from_byte_array([txid_seed; 32]), completion_block);
        let blob = DaBlob {
            spec_version,
            update_seq_no,
            evm_header: EvmHeaderSummary {
                block_num,
                timestamp: block_num + 1_700_000_000,
                base_fee: 100,
                gas_used: 21_000,
                gas_limit: 36_000_000,
                da_rate: HeaderExtra::new(spec_version, 2_500_000_000).da_rate(),
            },
            state_diff: Default::default(),
        };
        RecoveredDaBlob::new(l1_ref, blob)
    }

    fn assert_blob_eq(actual: &RecoveredDaBlob, expected: &RecoveredDaBlob) {
        assert_eq!(actual.l1_ref(), expected.l1_ref());
        assert_eq!(actual.blob().spec_version, expected.blob().spec_version);
        assert_eq!(
            actual.blob().encode_to_vec().expect("encode actual blob"),
            expected
                .blob()
                .encode_to_vec()
                .expect("encode expected blob"),
        );
    }

    #[test]
    fn test_recovered_blobs_roundtrip_layout_payload_and_provenance() {
        let (_datadir, database) = setup_db();
        let blobs = vec![
            build_blob_for_spec(AlpenSpecId::V0, 4, 40, 2, make_block(100, 1)),
            build_blob_for_spec(AlpenSpecId::V1, 5, 50, 3, make_block(101, 2)),
        ];

        database.put(blobs.clone()).expect("store recovered blobs");

        let loaded = database
            .get_contiguous_from(4, 101)
            .expect("load recovered blobs");
        assert_eq!(loaded.len(), blobs.len());
        for (actual, expected) in loaded.iter().zip(&blobs) {
            assert_blob_eq(actual, expected);
        }
    }

    #[test]
    fn test_contiguous_read_stops_at_sequence_gap() {
        let (_datadir, database) = setup_db();
        let seq_ten = build_blob(10, 100, 10, make_block(100, 1));
        let seq_two = build_blob(2, 20, 2, make_block(101, 2));

        database
            .put(vec![seq_ten, seq_two])
            .expect("store recovered blobs");

        let loaded = database
            .get_contiguous_from(2, 101)
            .expect("load contiguous recovered blobs");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].blob().update_seq_no, 2);

        let loaded = database
            .get_contiguous_from(10, 101)
            .expect("load later contiguous recovered blobs");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].blob().update_seq_no, 10);
    }

    #[test]
    fn test_distinct_commits_for_same_sequence_are_preserved() {
        let (_datadir, database) = setup_db();
        let block = make_block(100, 1);
        let first = build_blob(4, 40, 2, block);
        let second = build_blob(4, 41, 3, block);

        database
            .put(vec![first.clone(), second.clone()])
            .expect("store duplicate-sequence recovered blobs");

        let loaded = database
            .get_contiguous_from(4, 101)
            .expect("load duplicate-sequence recovered blobs");
        assert_eq!(loaded.len(), 2);
        assert_blob_eq(&loaded[0], &first);
        assert_blob_eq(&loaded[1], &second);
    }

    #[test]
    fn test_identical_redelivery_is_idempotent_and_conflict_is_rejected() {
        let (_datadir, database) = setup_db();
        let block = make_block(100, 1);
        let blob = build_blob(4, 40, 2, block);

        database
            .put(vec![blob.clone()])
            .expect("store recovered blob");
        database
            .put(vec![blob])
            .expect("redeliver identical recovered blob");

        let error = database
            .put(vec![
                build_blob(5, 50, 3, make_block(101, 2)),
                build_blob(4, 41, 2, block),
            ])
            .expect_err("conflicting redelivery must fail");
        assert!(matches!(
            error,
            RecoveredDaDbError::BlobConflict {
                update_seq_no: 4,
                ..
            }
        ));

        let loaded = database
            .get_contiguous_from(4, 100)
            .expect("load persisted recovered blob");
        assert_eq!(
            loaded.len(),
            1,
            "the failed write must roll back atomically"
        );
        assert_eq!(loaded[0].blob().evm_header.block_num, 40);
    }

    #[test]
    fn test_prune_below_preserves_boundary_and_later_blobs() {
        let (_datadir, database) = setup_db();
        database
            .put(vec![
                build_blob(4, 40, 4, make_block(100, 1)),
                build_blob(5, 50, 5, make_block(101, 2)),
                build_blob(6, 60, 6, make_block(102, 3)),
            ])
            .expect("store recovered blobs");

        database.prune_before(5).expect("prune recovered blobs");

        let loaded = database
            .get_contiguous_from(5, 102)
            .expect("load unpruned recovered blobs");
        let update_seq_nos = loaded
            .iter()
            .map(|blob| blob.blob().update_seq_no)
            .collect::<Vec<_>>();
        assert_eq!(update_seq_nos, vec![5, 6]);
    }

    #[test]
    fn test_clear_removes_all_recovered_blobs() {
        let (_datadir, database) = setup_db();
        database
            .put(vec![
                build_blob(4, 40, 4, make_block(100, 1)),
                build_blob(5, 50, 5, make_block(101, 2)),
            ])
            .expect("store recovered blobs");

        database.clear().expect("clear recovered blobs");

        assert!(database
            .get_contiguous_from(4, 101)
            .expect("load recovered blobs")
            .is_empty());
    }

    #[test]
    fn test_contiguous_read_excludes_candidates_above_l1_frontier() {
        let (_datadir, database) = setup_db();
        let eligible = build_blob(4, 40, 4, make_block(100, 1));
        let future_duplicate = build_blob(4, 41, 5, make_block(102, 2));
        let future_next = build_blob(5, 50, 6, make_block(102, 3));
        database
            .put(vec![future_next, future_duplicate, eligible.clone()])
            .expect("store recovered blobs");

        let loaded = database
            .get_contiguous_from(4, 100)
            .expect("load frontier-eligible prefix");

        assert_eq!(loaded.len(), 1);
        assert_blob_eq(&loaded[0], &eligible);
    }
}
