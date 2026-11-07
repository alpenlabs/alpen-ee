//! The sequencer's prover programs by key, and the spec version a predicate
//! rotation activates.

use alpen_params::AlpenSpecId;
use strata_predicate::PredicateKey;
use thiserror::Error;

/// The sequencer's prover programs, each by its account predicate key, with
/// the spec version it proves.
///
/// A consumed rotation names a key, and the block after it runs under the
/// version of that key's program. So a rotation to a new key for unchanged
/// rules keeps the version, and a rotation to a later version's program moves
/// to that version.
///
/// The versions come from the operator's config. Each has to match the
/// version built into its program's guest, and only the proof checks that:
/// the guest rejects blocks stamped with any other version. So a wrong one
/// leaves the blocks after the rotation unprovable, not wrongly proved.
#[derive(Debug, Clone, Default)]
pub struct ProgramRegistry {
    programs: Vec<(PredicateKey, AlpenSpecId)>,
}

/// Why a rotation can't be honored, or why the registry can't be built.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProgramRegistryError {
    /// The rotation names a key that no prover program of this sequencer has.
    #[error("no prover program has the rotation's key {key}")]
    UnknownKey { key: PredicateKey },

    /// The key's program proves a version older than the one in force. Full
    /// nodes reject a block whose version goes backwards.
    #[error("the rotation's key {key} proves {target}, older than the current {current}")]
    Regression {
        key: PredicateKey,
        target: AlpenSpecId,
        current: AlpenSpecId,
    },

    /// Two programs have the same key, so the key can't name one version.
    #[error("two prover programs have the key {key}")]
    DuplicateKey { key: PredicateKey },
}

impl ProgramRegistry {
    /// Builds the registry from each program's key and spec version.
    pub fn new(
        programs: impl IntoIterator<Item = (PredicateKey, AlpenSpecId)>,
    ) -> Result<Self, ProgramRegistryError> {
        let mut table: Vec<(PredicateKey, AlpenSpecId)> = Vec::new();
        for (key, spec_version) in programs {
            if table.iter().any(|(known, _)| *known == key) {
                return Err(ProgramRegistryError::DuplicateKey { key });
            }
            table.push((key, spec_version));
        }
        Ok(Self { programs: table })
    }

    /// Returns the spec version of `key`'s program, if the sequencer has one.
    pub fn spec_version(&self, key: &PredicateKey) -> Option<AlpenSpecId> {
        self.programs
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, spec_version)| *spec_version)
    }

    /// Returns the version the block after a rotation to `key` runs under,
    /// when the block consuming the rotation runs under `current`.
    pub fn activated_version(
        &self,
        key: &PredicateKey,
        current: AlpenSpecId,
    ) -> Result<AlpenSpecId, ProgramRegistryError> {
        let target = self
            .spec_version(key)
            .ok_or_else(|| ProgramRegistryError::UnknownKey { key: key.clone() })?;
        if target < current {
            return Err(ProgramRegistryError::Regression {
                key: key.clone(),
                target,
                current,
            });
        }
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use strata_predicate::PredicateTypeId;

    use super::*;

    fn key(byte: u8) -> PredicateKey {
        PredicateKey::try_new(PredicateTypeId::Bip340Schnorr, vec![byte; 32]).unwrap()
    }

    #[test]
    fn a_new_key_for_the_same_version_keeps_the_version() {
        let registry =
            ProgramRegistry::new([(key(1), AlpenSpecId::V2), (key(2), AlpenSpecId::V2)]).unwrap();

        assert_eq!(
            registry.activated_version(&key(2), AlpenSpecId::V2),
            Ok(AlpenSpecId::V2)
        );
    }

    #[test]
    fn a_later_version_key_moves_to_that_version() {
        let registry =
            ProgramRegistry::new([(key(1), AlpenSpecId::V0), (key(2), AlpenSpecId::V2)]).unwrap();

        // Skipping V1 is allowed: the version comes from the key, not from
        // counting rotations.
        assert_eq!(
            registry.activated_version(&key(2), AlpenSpecId::V0),
            Ok(AlpenSpecId::V2)
        );
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let registry = ProgramRegistry::new([(key(1), AlpenSpecId::V1)]).unwrap();

        assert_eq!(
            registry.activated_version(&key(9), AlpenSpecId::V1),
            Err(ProgramRegistryError::UnknownKey { key: key(9) })
        );
    }

    #[test]
    fn an_older_version_key_is_refused() {
        let registry =
            ProgramRegistry::new([(key(1), AlpenSpecId::V1), (key(2), AlpenSpecId::V2)]).unwrap();

        assert_eq!(
            registry.activated_version(&key(1), AlpenSpecId::V2),
            Err(ProgramRegistryError::Regression {
                key: key(1),
                target: AlpenSpecId::V1,
                current: AlpenSpecId::V2,
            })
        );
    }

    #[test]
    fn a_key_listed_twice_is_refused() {
        let err = ProgramRegistry::new([(key(1), AlpenSpecId::V1), (key(1), AlpenSpecId::V2)])
            .unwrap_err();

        assert_eq!(err, ProgramRegistryError::DuplicateKey { key: key(1) });
    }
}
