//! EE database initialization.
//!
//! Opens the EE stores under `<datadir>/mdbx` as MDBX environments, mirroring
//! the role split of the previous layout: every node opens the node store
//! via [`EeDb::node_storage`], and only a sequencer additionally opens the
//! DA/witness/prover stores via [`EeDb::sequencer_databases`]. Each store is an
//! isolated environment (independent write-locks); the DA-context filter shares
//! the witness environment, and the L1 broadcast and chunked-envelope stores
//! share the DA environment.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use alpen_db_store_mdbx::{MdbxConfig, MdbxEnv};
use alpen_reth_db::mdbx::{witness_tables, EeDaContextDbMdbx, WitnessDbMdbx};
use eyre::{eyre, Result};
/// Re-export the async ops proxies so callers do not need `strata-storage`.
pub use strata_db_types::chunked_envelope::L1ChunkedEnvelopeDatabaseProxy as ChunkedEnvelopeOps;
pub use strata_db_types::l1_broadcast::L1BroadcastDatabaseProxy as BroadcastDbOps;
use tokio::runtime::Handle;

use crate::{
    mdbxdb::{da_tables, EeNodeDbMdbx, EeProverDbMdbx, L1BroadcastDbMdbx, L1ChunkedEnvelopeDbMdbx},
    storage::EeNodeStorage,
};

/// An opened set of EE MDBX environments, from which databases are created per
/// role.
///
/// The node store is opened eagerly (every node keeps chain state); the
/// sequencer stores are opened on demand in [`EeDb::sequencer_databases`], so a
/// full node never materializes them.
#[derive(Debug)]
pub struct EeDb {
    mdbx_dir: PathBuf,
    config: MdbxConfig,
    node_db: Arc<EeNodeDbMdbx>,
}

/// Opens the EE MDBX environments under `<datadir>/mdbx`.
///
/// Returns a handle from which each role takes the databases it needs:
/// [`EeDb::node_storage`] for chain state, and [`EeDb::sequencer_databases`] for
/// the DA and prover databases only a sequencer uses.
///
/// `_db_retry_count` is accepted for compatibility with the previous layout but
/// unused: MDBX serializes writers, so there is no optimistic-retry backoff.
pub fn open_ee_db(datadir: &Path, _db_retry_count: u16) -> Result<EeDb> {
    let mdbx_dir = datadir.join("mdbx");
    let config = MdbxConfig::default();
    let node_db = Arc::new(
        EeNodeDbMdbx::open(&mdbx_dir.join("node"), &config)
            .map_err(|e| eyre!("failed to open EE node db: {e}"))?,
    );
    Ok(EeDb {
        mdbx_dir,
        config,
        node_db,
    })
}

impl EeDb {
    /// Creates [`EeNodeStorage`] over the EE node database, dispatching blocking
    /// work via the given runtime handle.
    ///
    /// This is the chain state every node keeps, whatever its role.
    pub fn node_storage(&self, handle: Handle) -> Result<EeNodeStorage> {
        Ok(EeNodeStorage::new(handle, self.node_db.clone()))
    }

    /// Opens and returns the databases only a sequencer uses.
    ///
    /// A full node should not call this, so its environments are never created.
    pub fn sequencer_databases(&self) -> Result<SequencerDatabases> {
        let prover_db = Arc::new(
            EeProverDbMdbx::open(&self.mdbx_dir.join("prover"), &self.config)
                .map_err(|e| eyre!("failed to open EE prover db: {e}"))?,
        );

        let witness_env = Arc::new(
            MdbxEnv::open(
                &self.mdbx_dir.join("witness"),
                &self.config,
                &witness_tables(),
            )
            .map_err(|e| eyre!("failed to open EE witness env: {e}"))?,
        );
        let witness_db = Arc::new(WitnessDbMdbx::new(witness_env.clone()));
        let da_context_db = Arc::new(EeDaContextDbMdbx::new(witness_env, witness_db.clone()));

        // L1 broadcast + chunked-envelope share one DA environment.
        let da_env = Arc::new(
            MdbxEnv::open(&self.mdbx_dir.join("da"), &self.config, &da_tables())
                .map_err(|e| eyre!("failed to open EE DA env: {e}"))?,
        );
        let broadcast_db = Arc::new(L1BroadcastDbMdbx::new(da_env.clone()));
        let chunked_envelope_db = Arc::new(L1ChunkedEnvelopeDbMdbx::new(da_env));

        Ok(SequencerDatabases {
            witness_db,
            broadcast_db,
            chunked_envelope_db,
            da_context_db,
            prover_db,
        })
    }
}

/// The databases only a sequencer touches: DA witnesses, the L1 broadcast and
/// chunked-envelope queues, the cross-batch DA dedup context, and prover-side
/// persistence.
///
/// Separate from the node state in [`EeDb::node_storage`] so a full node neither
/// holds nor creates them.
#[derive(Debug)]
pub struct SequencerDatabases {
    /// Witness database for state diffs and block witnesses.
    witness_db: Arc<WitnessDbMdbx>,
    /// L1 broadcast transaction database.
    broadcast_db: Arc<L1BroadcastDbMdbx>,
    /// Chunked envelope database.
    chunked_envelope_db: Arc<L1ChunkedEnvelopeDbMdbx>,
    /// DA filter for cross-batch deduplication (shares the witness env).
    da_context_db: Arc<EeDaContextDbMdbx<WitnessDbMdbx>>,
    /// Prover-side persistence: shared task store + chunk receipts + acct proofs.
    prover_db: Arc<EeProverDbMdbx>,
}

impl SequencerDatabases {
    /// Returns a clone of the witness database.
    pub fn witness_db(&self) -> Arc<WitnessDbMdbx> {
        self.witness_db.clone()
    }

    /// Creates [`BroadcastDbOps`] from the broadcast database.
    pub fn broadcast_ops(&self, handle: Handle) -> BroadcastDbOps {
        BroadcastDbOps::new(handle, self.broadcast_db.clone())
    }

    /// Creates [`ChunkedEnvelopeOps`] from the chunked envelope database.
    pub fn chunked_envelope_ops(&self, handle: Handle) -> ChunkedEnvelopeOps {
        ChunkedEnvelopeOps::new(handle, self.chunked_envelope_db.clone())
    }

    /// Returns a clone of the DA context database.
    pub fn da_context_db(&self) -> Arc<EeDaContextDbMdbx<WitnessDbMdbx>> {
        self.da_context_db.clone()
    }

    /// Returns a clone of the prover database (shared task store + chunk
    /// receipts + acct proofs).
    pub fn prover_db(&self) -> Arc<EeProverDbMdbx> {
        self.prover_db.clone()
    }
}

/// Opens the DA broadcast + chunked-envelope stores over one env at `datadir`
/// and wraps them in their async ops proxies.
///
/// For downstream tests that need working DA stores without the full node
/// database; `handle` dispatches the proxies' blocking work.
#[cfg(feature = "test-utils")]
pub fn open_da_ops(datadir: &Path, handle: Handle) -> Result<(BroadcastDbOps, ChunkedEnvelopeOps)> {
    let da_env = Arc::new(
        MdbxEnv::open(
            &datadir.join("mdbx").join("da"),
            &MdbxConfig::default(),
            &da_tables(),
        )
        .map_err(|e| eyre!("failed to open EE DA env: {e}"))?,
    );
    let broadcast_db = Arc::new(L1BroadcastDbMdbx::new(da_env.clone()));
    let chunked_envelope_db = Arc::new(L1ChunkedEnvelopeDbMdbx::new(da_env));
    Ok((
        BroadcastDbOps::new(handle.clone(), broadcast_db),
        ChunkedEnvelopeOps::new(handle, chunked_envelope_db),
    ))
}
