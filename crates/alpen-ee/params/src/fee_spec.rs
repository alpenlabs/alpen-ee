//! Fee settings of each Alpen spec version.

use serde::{Deserialize, Serialize};

use crate::{AlpenSpecId, SpecVersioned};

/// Fee settings of each spec version.
///
/// Fee rules are consensus rules. Payload construction and full-node header validation must
/// apply the same ones, so they live in the signed params artifact. Each setting is a
/// [`SpecVersioned`] value, so a network can change it only at a version boundary.
///
/// It holds only the base-fee floor today. It is meant to grow: a new fee setting gets its own
/// [`SpecVersioned`] field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeeSpec {
    /// Minimum EIP-1559 base fee per gas, in wei.
    ///
    /// A zero floor is plain EIP-1559. Deployed chains ran that under [`AlpenSpecId::V0`]
    /// before the floor existed, so their artifacts keep V0's floor at zero. The EEST
    /// conformance environment keeps every version at zero.
    base_fee_floor: SpecVersioned<u64>,
}

impl FeeSpec {
    /// Creates fee settings from each setting's per-version values.
    pub fn new(base_fee_floor: SpecVersioned<u64>) -> Self {
        Self { base_fee_floor }
    }

    /// Returns the minimum EIP-1559 base fee per gas, in wei, of a block under `version`.
    pub fn base_fee_floor(&self, version: AlpenSpecId) -> u64 {
        *self.base_fee_floor.get(version)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::FeeSpec;
    use crate::AlpenSpecId;

    #[test]
    fn reads_each_versions_floor() {
        let fee_spec: FeeSpec =
            serde_json::from_value(json!({"base_fee_floor": {"v0": 0, "v1": 1_000_000_000}}))
                .unwrap();

        assert_eq!(fee_spec.base_fee_floor(AlpenSpecId::V0), 0);
        assert_eq!(fee_spec.base_fee_floor(AlpenSpecId::V1), 1_000_000_000);
    }

    #[test]
    fn rejects_unknown_settings() {
        let json = json!({"base_fee_floor": {"v0": 0}, "priority_fee_floor": {"v0": 0}});
        assert!(serde_json::from_value::<FeeSpec>(json).is_err());
    }

    #[test]
    fn rejects_a_missing_floor() {
        assert!(serde_json::from_value::<FeeSpec>(json!({})).is_err());
    }
}
