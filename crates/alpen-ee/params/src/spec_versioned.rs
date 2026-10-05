//! Params values that can change at a spec version boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::AlpenSpecId;

/// A value of each spec version.
///
/// A version with no entry keeps its predecessor's value. So an entry appears only where the
/// value changes, and a new [`AlpenSpecId`] variant needs no new entry. [`AlpenSpecId::V0`]
/// must have one, because every later version inherits from it. Serializes as a map from
/// version name to value, e.g. `{"v0": 0, "v1": 1000000000}`.
///
/// Changing the value of a version that has already activated changes the rules of blocks that
/// already exist, and splits nodes on the old and new artifacts. Change a value only by adding
/// an entry for a version that has not activated yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<AlpenSpecId, T>",
    into = "BTreeMap<AlpenSpecId, T>"
)]
pub struct SpecVersioned<T: Clone>(BTreeMap<AlpenSpecId, T>);

/// A [`SpecVersioned`] map with no entry for [`AlpenSpecId::V0`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("v0 has no entry; every later version inherits its value from v0")]
pub struct MissingV0Entry;

impl<T: Clone> SpecVersioned<T> {
    /// Creates a value that is `value` under every version.
    pub fn new(value: T) -> Self {
        Self(BTreeMap::from([(AlpenSpecId::V0, value)]))
    }

    /// Sets the value from `version` on, up to the next version with its own entry.
    pub fn with(mut self, version: AlpenSpecId, value: T) -> Self {
        self.0.insert(version, value);
        self
    }

    /// Returns the value under `version`: its own entry, or else the entry of the newest version
    /// before it.
    pub fn get(&self, version: AlpenSpecId) -> &T {
        let (_, value) = self
            .0
            .range(..=version)
            .next_back()
            .expect("SpecVersioned invariant: v0 has an entry");
        value
    }
}

impl<T: Clone> TryFrom<BTreeMap<AlpenSpecId, T>> for SpecVersioned<T> {
    type Error = MissingV0Entry;

    fn try_from(entries: BTreeMap<AlpenSpecId, T>) -> Result<Self, Self::Error> {
        if !entries.contains_key(&AlpenSpecId::V0) {
            return Err(MissingV0Entry);
        }
        Ok(Self(entries))
    }
}

impl<T: Clone> From<SpecVersioned<T>> for BTreeMap<AlpenSpecId, T> {
    fn from(value: SpecVersioned<T>) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::SpecVersioned;
    use crate::AlpenSpecId;

    #[test]
    fn version_without_entry_inherits_its_predecessors_value() {
        let only_v0 = SpecVersioned::new(7u64);
        assert_eq!(*only_v0.get(AlpenSpecId::V0), 7);
        assert_eq!(*only_v0.get(AlpenSpecId::V1), 7);

        let raised_at_v1 = only_v0.with(AlpenSpecId::V1, 9);
        assert_eq!(*raised_at_v1.get(AlpenSpecId::V0), 7);
        assert_eq!(*raised_at_v1.get(AlpenSpecId::V1), 9);
    }

    #[test]
    fn json_keeps_the_entries_as_written() {
        let values = SpecVersioned::new(0u64).with(AlpenSpecId::V1, 1_000_000_000);
        let json = json!({"v0": 0, "v1": 1_000_000_000});

        assert_eq!(serde_json::to_value(&values).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<SpecVersioned<u64>>(json).unwrap(),
            values
        );
    }

    #[test]
    fn json_rejects_a_map_without_v0() {
        let err = serde_json::from_value::<SpecVersioned<u64>>(json!({"v1": 1})).unwrap_err();
        assert!(err.to_string().contains("v0 has no entry"), "{err}");
    }

    #[test]
    fn json_rejects_unknown_versions() {
        assert!(serde_json::from_value::<SpecVersioned<u64>>(json!({"v0": 0, "v9": 1})).is_err());
    }
}
