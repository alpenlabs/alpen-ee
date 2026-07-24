//! Pending upgrade declaration.
//!
//! An Alpen EE upgrade is gated by the account's VK rotation: the batch that
//! consumes the VK-update message is the last one under the old rules, and
//! the next block is the first one under the new rules (see the Alpen
//! upgrade strategy design). The fork activation coordinate therefore cannot
//! be written into the params artifact up front — it is derived at runtime
//! from where the VK-update message lands in the inbox ordering.
//!
//! What the artifact *can* declare is which forks the rollout carries: the
//! upgraded binary ships with the new logic disabled, and [`PendingUpgrade`]
//! names the forks that must activate at the next VK-update boundary.

use core::fmt;
use std::str::FromStr;

use reth_ethereum_forks::EthereumHardfork;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Error decoding a [`PendingUpgrade`].
#[derive(Debug, thiserror::Error)]
pub enum PendingUpgradeError {
    /// The named fork is not a known EVM hardfork.
    #[error("unknown EVM fork name `{0}`")]
    UnknownFork(String),

    /// The fork is not timestamp-scheduled (pre-Shanghai), so it cannot be
    /// activated at a runtime-derived boundary.
    #[error(
        "EVM fork `{0}` is not timestamp-scheduled; only Shanghai-or-later forks can be pending"
    )]
    NotTimestampScheduled(EthereumHardfork),
}

/// A stock EVM hardfork named in the params artifact.
///
/// Serialized as the fork's name (case-insensitive on decode, e.g.
/// `"osaka"`). Decoding validates that the fork is timestamp-scheduled
/// (Shanghai or later): the runtime-derived activation is encoded as a
/// `ForkCondition::Timestamp`, which older block-scheduled forks never
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmForkName(EthereumHardfork);

impl EvmForkName {
    /// Creates a pending EVM fork name, validating it is timestamp-scheduled.
    pub fn new(fork: EthereumHardfork) -> Result<Self, PendingUpgradeError> {
        if fork < EthereumHardfork::Shanghai {
            return Err(PendingUpgradeError::NotTimestampScheduled(fork));
        }
        Ok(Self(fork))
    }

    /// Returns the underlying hardfork.
    pub fn fork(&self) -> EthereumHardfork {
        self.0
    }
}

impl fmt::Display for EvmForkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for EvmForkName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for EvmForkName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        let fork = EthereumHardfork::from_str(&name)
            .map_err(|_| de::Error::custom(PendingUpgradeError::UnknownFork(name)))?;
        EvmForkName::new(fork).map_err(de::Error::custom)
    }
}

/// The forks this rollout activates at the next VK-update boundary.
///
/// The upgraded node runs with these forks disabled until it observes the
/// VK-update message in the EE inbox ordering; the boundary derivation then
/// schedules every listed fork at the first post-boundary block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingUpgrade {
    /// Stock EVM hardforks to activate at the boundary.
    #[serde(default)]
    evm_forks: Vec<EvmForkName>,
}

impl PendingUpgrade {
    /// Creates a new pending upgrade declaration.
    pub fn new(evm_forks: Vec<EvmForkName>) -> Self {
        Self { evm_forks }
    }

    /// Returns the pending stock EVM forks.
    pub fn evm_forks(&self) -> &[EvmForkName] {
        &self.evm_forks
    }

    /// Returns whether the declaration carries no forks.
    pub fn is_empty(&self) -> bool {
        self.evm_forks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use reth_ethereum_forks::EthereumHardfork;

    use super::{EvmForkName, PendingUpgrade};

    #[test]
    fn fork_name_roundtrips_case_insensitively() {
        let upgrade: PendingUpgrade =
            serde_json::from_str(r#"{"evm_forks":["osaka"]}"#).expect("osaka is a valid fork");
        assert_eq!(
            upgrade.evm_forks(),
            &[EvmForkName::new(EthereumHardfork::Osaka).expect("osaka is timestamp-scheduled")]
        );

        let json = serde_json::to_string(&upgrade).expect("upgrade should serialize");
        let decoded: PendingUpgrade = serde_json::from_str(&json).expect("upgrade should reparse");
        assert_eq!(decoded, upgrade);
    }

    #[test]
    fn unknown_fork_name_is_rejected() {
        assert!(serde_json::from_str::<PendingUpgrade>(r#"{"evm_forks":["tokyo"]}"#).is_err());
    }

    #[test]
    fn block_scheduled_fork_is_rejected() {
        assert!(serde_json::from_str::<PendingUpgrade>(r#"{"evm_forks":["london"]}"#).is_err());
    }
}
