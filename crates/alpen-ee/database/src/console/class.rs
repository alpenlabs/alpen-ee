//! Restoration classes for EE tables.
//!
//! The class of a table fixes the blast radius of a bad write and the recovery
//! path afterward (see the `ee-db-console` design doc §8 and `ee-storage-layer`
//! §6). The console surfaces the class in `.tables`/`.schema` and, once the
//! write path lands, gates Class-U edits behind an extra confirmation.

use std::fmt;

/// The restoration class of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableClass {
    /// Tier-1 canonical log — the shipped source of truth. Never writable from
    /// the console.
    Canonical,
    /// Index — rebuildable by rescanning Tier 1.
    Index,
    /// Proving cache — regenerable by re-execution.
    ProvingCache,
    /// Operational — replayable from L1 (DA) or the prover.
    Operational,
    /// Unfinalized tip — locally authoritative and **not** rebuildable from
    /// Tier 1; a wrong edit is real data loss, so it gets the loudest guard.
    Unfinalized,
}

impl TableClass {
    /// The single-letter code shown in `.tables` / `.schema` (`C`/`I`/`R`/`O`/`U`).
    pub fn code(self) -> char {
        match self {
            Self::Canonical => 'C',
            Self::Index => 'I',
            Self::ProvingCache => 'R',
            Self::Operational => 'O',
            Self::Unfinalized => 'U',
        }
    }

    /// Whether a console write to this class is recoverable after a mistake.
    pub fn is_recoverable(self) -> bool {
        !matches!(self, Self::Canonical | Self::Unfinalized)
    }

    /// Whether writes to this class require the loud extra confirmation.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::Unfinalized)
    }
}

impl fmt::Display for TableClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "class {}", self.code())
    }
}
