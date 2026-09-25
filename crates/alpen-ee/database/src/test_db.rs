//! A throwaway EE datadir with a row in every table, for tests of tooling
//! that reads the store.
//!
//! Seeding goes through the node's own write paths, so a test sees exactly
//! what production code stores rather than a hand-built approximation of it.
//! [`TempDatadir`] owns the directory and removes it on drop; keep it alive
//! for as long as a store under it is open.

use std::{
    env, fs,
    ops::Deref,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use alloy_primitives::B256;
use alpen_ee_common::{
    exec_block_storage_test_fns::create_exec_block, AccessedAccount, AccessedStateRecord, Batch,
    Chunk,
};
use alpen_reth_db::{
    mdbx::{witness_tables, EeDaContextDbMdbx, WitnessDbMdbx},
    EeDaContext, StateDiffStore,
};
use alpen_reth_statediff::BlockStateChanges;
use alpen_store_mdbx::{MdbxConfig, MdbxEnv};
use strata_acct_types::Hash;
use strata_db_types::{
    chunked_envelope::{ChunkedEnvelopeEntry, L1ChunkedEnvelopeDatabase},
    fee_bump::{TxAttempt, TxAttemptParts, TxAttemptStatus, TxNodeKind, TxNodeRecord},
    l1_broadcast::{L1BroadcastDatabase, L1TxEntry},
};
use strata_ee_acct_types::EeAccountState;
use strata_identifiers::{Buf32, EpochCommitment, OLBlockId, RBuf32};
use strata_l1_txfmt::MagicBytes;
use strata_paas::{TaskRecordData, TaskStatus};
use zkaleido::{
    ProgramId, Proof, ProofMetadata, ProofReceipt, ProofReceiptWithMetadata, ProofType,
    PublicValues, ZkVm,
};

use crate::{
    database::EeNodeDb,
    mdbxdb::{
        da_tables, prover_tables, AcctProofIdIndexSchema, AcctProofReceiptSchema,
        ChunkProofReceiptSchema, EeNodeDbMdbx, L1BroadcastDbMdbx, L1ChunkedEnvelopeDbMdbx,
        ProverTaskSchema,
    },
    serialization_types::DBBatchId,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique datadir path under the system temp directory, removed on drop.
///
/// The directory itself is created by whatever opens a store under it.
#[derive(Debug)]
pub struct TempDatadir(PathBuf);

impl TempDatadir {
    /// A path nothing has written to yet.
    pub fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(env::temp_dir().join(format!("alpen-ee-test-{}-{unique}", process::id())))
    }

    /// A datadir with a row in every table of every environment.
    pub fn seeded() -> Self {
        let datadir = Self::new();
        seed_all(&datadir);
        datadir
    }
}

impl Default for TempDatadir {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for TempDatadir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDatadir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDatadir {
    fn drop(&mut self) {
        // Nothing may have been written under it.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hash(seed: u8) -> Hash {
    Hash::from([seed; 32])
}

/// The node environment, through the node's own storage trait.
pub fn seed_node(datadir: &Path) {
    let db = EeNodeDbMdbx::open(&datadir.join("mdbx").join("node"), &MdbxConfig::small()).unwrap();
    let (h0, h1, h2) = (hash(1), hash(2), hash(3));

    db.save_exec_block(create_exec_block(0, Hash::default(), h0, 0), vec![0xaa; 8])
        .unwrap();
    db.save_exec_block(create_exec_block(1, h0, h1, 1), vec![0xbb; 8])
        .unwrap();
    db.save_exec_block(create_exec_block(2, h1, h2, 2), vec![0xcc; 8])
        .unwrap();
    db.init_finalized_chain(h0).unwrap();
    db.extend_finalized_chain(h2).unwrap();

    let batch = Batch::new_genesis_batch(h0, 0).unwrap();
    let batch_id = batch.id();
    db.save_genesis_batch(batch).unwrap();
    let chunk = Chunk::new(0, h0, h2, 2, 0, vec![h1, h2]);
    let chunk_id = chunk.id();
    db.save_next_chunk(chunk).unwrap();
    db.set_batch_chunks(batch_id, vec![chunk_id]).unwrap();

    let epoch = EpochCommitment::new(1, 5, OLBlockId::from(Buf32::from([9u8; 32])));
    db.store_ee_account_state(epoch, EeAccountState::new(h2, hash(3), vec![], vec![]))
        .unwrap();

    let record = AccessedStateRecord::new(
        vec![AccessedAccount::new([1u8; 20], vec![[2u8; 32]])],
        vec![[4u8; 32]],
        vec![0],
    );
    db.put_block_accessed_state(h1, record).unwrap();
    db.put_bytecode(hash(4), vec![0x60, 0x00]).unwrap();
    db.put_block_witness(h1, vec![1, 2, 3]).unwrap();
}

/// The prover environment: the task store and the receipt tables.
pub fn seed_prover(datadir: &Path) {
    let env = MdbxEnv::open(
        &datadir.join("mdbx").join("prover"),
        &MdbxConfig::small(),
        &prover_tables(),
    )
    .unwrap();
    let receipt = ProofReceiptWithMetadata::new(
        ProofReceipt::new(Proof::new(vec![7; 64]), PublicValues::new(vec![8; 16])),
        ProofMetadata::new(ZkVm::Native, ProgramId([1; 32]), "0.1", ProofType::Groth16),
    );
    let batch_id: DBBatchId = Batch::new_genesis_batch(hash(1), 0).unwrap().id().into();
    env.update(|w| {
        w.put::<ProverTaskSchema>(&vec![1, 2, 3], &TaskRecordData::new(TaskStatus::Pending))?;
        w.put::<ChunkProofReceiptSchema>(&vec![4, 5, 6], &receipt)?;
        w.put::<AcctProofReceiptSchema>(&batch_id, &receipt)?;
        w.put::<AcctProofIdIndexSchema>(&hash(5), &batch_id)
    })
    .unwrap();
}

/// The witness environment, through the reth-side store traits.
pub fn seed_witness(datadir: &Path) {
    let env = Arc::new(
        MdbxEnv::open(
            &datadir.join("mdbx").join("witness"),
            &MdbxConfig::small(),
            &witness_tables(),
        )
        .unwrap(),
    );
    let witness = Arc::new(WitnessDbMdbx::new(env.clone()));
    witness
        .put_state_diff(B256::from([6u8; 32]), 6, &BlockStateChanges::default())
        .unwrap();
    EeDaContextDbMdbx::new(env, witness)
        .mark_code_hashes_published(&[B256::from([7u8; 32])])
        .unwrap();
}

/// The DA environment, through the broadcast and envelope database traits.
pub fn seed_da(datadir: &Path) {
    let env = Arc::new(
        MdbxEnv::open(
            &datadir.join("mdbx").join("da"),
            &MdbxConfig::small(),
            &da_tables(),
        )
        .unwrap(),
    );
    let broadcast = L1BroadcastDbMdbx::new(env.clone());
    broadcast
        .put_tx_entry(
            Buf32::from([8u8; 32]),
            L1TxEntry::new_unpublished(vec![0xde, 0xad]),
        )
        .unwrap();
    let attempt = TxAttempt::new(
        TxAttemptParts {
            raw_tx: vec![0xbe, 0xef],
            txid: RBuf32([9u8; 32]),
            wtxid: RBuf32([10u8; 32]),
            fee_rate_sat_vb: 2,
            fee_sats: 200,
        },
        0,
        TxAttemptStatus::Active,
    );
    let node = TxNodeRecord::new(TxNodeKind::SingleEnvelopeCommit { payload_idx: 0 }, attempt);
    broadcast.put_tx_node(node.node_id, node).unwrap();

    L1ChunkedEnvelopeDbMdbx::new(env)
        .put_chunked_envelope_entry(
            0,
            ChunkedEnvelopeEntry::new_unsigned(
                vec![vec![1, 2], vec![3]],
                MagicBytes::new(*b"ALPN"),
                1,
            ),
        )
        .unwrap();
}

/// Seeds every environment. The datadir must not exist yet.
pub fn seed_all(datadir: &Path) {
    seed_node(datadir);
    seed_prover(datadir);
    seed_witness(datadir);
    seed_da(datadir);
}
