use std::{fs, path::Path, sync::Arc};

use alpen_reth_db::sled::{EeDaContextDb, WitnessDB as SledWitnessDB};
use eyre::{eyre, Context, Result};
use strata_db_store_sled::{
    broadcaster::db::L1BroadcastDBSled, chunked_envelope::L1ChunkedEnvelopeDBSled, SledDbConfig,
};
/// Re-export ops types for callers.
pub use strata_storage::ops::{
    chunked_envelope::ChunkedEnvelopeOps, l1tx_broadcast::BroadcastDbOps,
};
use tokio::runtime::Handle;
use typed_sled::SledDb;

use crate::{
    sleddb::{EeNodeDBSled, EeProverDbSled},
    storage::EeNodeStorage,
};

/// An opened sled instance, from which databases are created per role.
///
/// Sled holds an exclusive file lock on its directory, so the instance can
/// only be opened once per process. That open happens in [`crate::open_ee_db`];
/// callers then take just the databases their role needs — every node calls
/// [`EeDb::node_storage`], and only a sequencer additionally calls
/// [`EeDb::sequencer_databases`]. Trees are created on demand, so a full
/// node never materializes the sequencer trees at all.
#[derive(Debug)]
pub struct EeDb {
    sled: Arc<SledDb>,
    config: SledDbConfig,
}

impl EeDb {
    /// Creates [`EeNodeStorage`] over the EE node database, dispatching
    /// blocking work via the given runtime handle.
    ///
    /// This is the chain state every node keeps, whatever its role.
    pub fn node_storage(&self, handle: Handle) -> Result<EeNodeStorage> {
        let ee_node_db = Arc::new(
            EeNodeDBSled::new(self.sled.clone(), self.config.clone())
                .map_err(|e| eyre!("failed to create EE node db: {e}"))?,
        );
        Ok(EeNodeStorage::new(handle, ee_node_db))
    }

    /// Creates the databases only a sequencer uses.
    ///
    /// Calling this creates their trees, so a full node should not call it.
    pub fn sequencer_databases(&self) -> Result<SequencerDatabases> {
        let witness_db = Arc::new(
            SledWitnessDB::new(self.sled.clone())
                .map_err(|e| eyre!("failed to create witness db: {e}"))?,
        );

        let broadcast_db = Arc::new(
            L1BroadcastDBSled::new(self.sled.clone(), self.config.clone())
                .map_err(|e| eyre!("failed to create broadcast db: {e}"))?,
        );

        let chunked_envelope_db = Arc::new(
            L1ChunkedEnvelopeDBSled::new(self.sled.clone(), self.config.clone())
                .map_err(|e| eyre!("failed to create chunked envelope db: {e}"))?,
        );

        let prover_db = Arc::new(
            EeProverDbSled::new(self.sled.clone(), self.config.clone())
                .map_err(|e| eyre!("failed to create EE prover db: {e}"))?,
        );

        let da_context_db = Arc::new(
            EeDaContextDb::new(self.sled.clone(), witness_db.clone())
                .map_err(|e| eyre!("failed to create DA context db: {e}"))?,
        );

        Ok(SequencerDatabases {
            witness_db,
            broadcast_db,
            chunked_envelope_db,
            da_context_db,
            prover_db,
        })
    }
}

/// The databases only a sequencer touches: DA witnesses, the L1 broadcast
/// and chunked envelope queues, the cross-batch DA dedup context, and
/// prover-side persistence.
///
/// Separate from the node state in [`EeDb::node_storage`] so a full node
/// neither holds nor creates them.
#[derive(Debug)]
pub struct SequencerDatabases {
    /// Witness database for state diffs and block witnesses.
    pub(crate) witness_db: Arc<SledWitnessDB>,
    /// L1 broadcast transaction database.
    pub(crate) broadcast_db: Arc<L1BroadcastDBSled>,
    /// Chunked envelope database.
    pub(crate) chunked_envelope_db: Arc<L1ChunkedEnvelopeDBSled>,
    /// DA filter for cross-batch deduplication (bytecodes, extensible for addresses etc.).
    pub(crate) da_context_db: Arc<EeDaContextDb<SledWitnessDB>>,
    /// Prover-side persistence: shared task store + chunk receipts + acct proofs.
    pub(crate) prover_db: Arc<EeProverDbSled>,
}

impl SequencerDatabases {
    /// Returns a clone of the witness database.
    pub fn witness_db(&self) -> Arc<SledWitnessDB> {
        self.witness_db.clone()
    }

    /// Creates [`BroadcastDbOps`] from the broadcast database, dispatching
    /// blocking work via the given runtime handle.
    pub fn broadcast_ops(&self, handle: Handle) -> BroadcastDbOps {
        BroadcastDbOps::new(handle, self.broadcast_db.clone())
    }

    /// Creates [`ChunkedEnvelopeOps`] from the chunked envelope database,
    /// dispatching blocking work via the given runtime handle.
    pub fn chunked_envelope_ops(&self, handle: Handle) -> ChunkedEnvelopeOps {
        ChunkedEnvelopeOps::new(handle, self.chunked_envelope_db.clone())
    }

    /// Returns a clone of the DA context database.
    pub fn da_context_db(&self) -> Arc<EeDaContextDb<SledWitnessDB>> {
        self.da_context_db.clone()
    }

    /// Returns a clone of the prover database (shared task store +
    /// chunk receipts + acct proofs).
    pub fn prover_db(&self) -> Arc<EeProverDbSled> {
        self.prover_db.clone()
    }
}

/// Opens the single sled instance at `<datadir>/sled`.
///
/// Databases are created from the returned [`EeDb`] per role. All typed-sled
/// trees coexist in one sled directory — each DB type uses uniquely named
/// trees so there are no collisions.
pub(crate) fn open_database(datadir: &Path, db_retry_count: u16) -> Result<EeDb> {
    let database_dir = datadir.join("sled");

    fs::create_dir_all(&database_dir)
        .wrap_err_with(|| format!("creating database directory at {database_dir:?}"))?;

    let sled_db = sled::open(&database_dir).wrap_err("opening sled database")?;

    let sled =
        Arc::new(SledDb::new(sled_db).map_err(|e| eyre!("failed to create typed sled db: {e}"))?);

    let retry_delay_ms = 200u64;
    let config = SledDbConfig::new_with_constant_backoff(db_retry_count, retry_delay_ms);

    Ok(EeDb { sled, config })
}
