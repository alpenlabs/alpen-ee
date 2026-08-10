use std::path::Path;

pub use sleddb::{EeDb, SequencerDatabases};

use crate::sleddb;

/// Opens the single sled instance at `<datadir>/sled`.
///
/// Returns a handle from which each role takes the databases it needs:
/// [`EeDb::node_storage`] for chain state, and [`EeDb::sequencer_databases`]
/// for the DA and prover databases only a sequencer uses. Sled locks its
/// directory exclusively, so this open must happen exactly once per process.
pub fn open_ee_db(datadir: &Path, db_retry_count: u16) -> eyre::Result<EeDb> {
    sleddb::open_database(datadir, db_retry_count)
}
