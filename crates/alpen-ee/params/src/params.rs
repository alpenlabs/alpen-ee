//! Top-level Alpen params artifact.

use std::sync::Arc;

use reth_chainspec::ChainSpec;
use serde::{Deserialize, Serialize};
use strata_acct_types::AccountId;
use strata_bridge_params::BridgeParams;
use strata_l1_txfmt::MagicBytes;

use crate::{
    genesis_info::AlpenEeGenesisBlockInfo, AlpenSpecId, AlpenSpecSchedule, BlobSpec, EvmSpec,
};

/// Default Alpen EE account id registered in generated OL params.
pub const DEFAULT_ALPEN_EE_ACCOUNT_ID: AccountId = AccountId::new([1u8; 32]);

/// Default minimum EIP-1559 base fee, in wei, used by existing Alpen networks.
pub const DEFAULT_BASE_FEE_FLOOR: u64 = 1_000_000_000;

const fn default_base_fee_floor() -> u64 {
    DEFAULT_BASE_FEE_FLOOR
}

/// Top-level Alpen chain params.
///
/// The single source of truth for how a node interprets the chain: the EE
/// account identity, bridge economics, DA stream identity, the Alpen spec
/// activations, fee policy, and the embedded EVM chain spec. Loaded from one
/// JSON artifact with validate-on-decode semantics on every field.
///
/// Unknown fields are rejected so that a params file written for a newer
/// node version (e.g. one carrying spec activations this binary does not
/// understand) fails loudly instead of being silently misread.
///
/// [`Default`] gives placeholder params (empty EVM genesis included) for
/// tests and benchmarks that need an `AlpenParams` but don't exercise EVM
/// execution — see the impl for what it contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlpenParams {
    /// Account id of the EE in OL. Fork-invariant.
    ///
    /// Named for the OL account system, not the EVM: do not confuse with an
    /// EVM address.
    strata_exec_account_id: AccountId,

    /// Bridge denomination and withdrawal policy.
    bridge_params: BridgeParams,

    /// DA stream identity.
    blob_spec: BlobSpec,

    /// Alpen spec activation schedule.
    #[serde(default)]
    spec_schedule: AlpenSpecSchedule,

    /// Minimum EIP-1559 base fee, in wei.
    ///
    /// Missing values retain the original 1 gwei protocol behavior so older
    /// params artifacts remain valid. Changing this value changes consensus
    /// and every node on a chain must use the same value.
    #[serde(default = "default_base_fee_floor")]
    base_fee_floor: u64,

    /// Embedded EVM chain spec (genesis document + fork configuration).
    evm_spec: EvmSpec,
}

impl AlpenParams {
    /// Creates new chain params.
    pub fn new(
        strata_exec_account_id: AccountId,
        bridge_params: BridgeParams,
        blob_spec: BlobSpec,
        spec_schedule: AlpenSpecSchedule,
        base_fee_floor: u64,
        evm_spec: EvmSpec,
    ) -> Self {
        Self {
            strata_exec_account_id,
            bridge_params,
            blob_spec,
            spec_schedule,
            base_fee_floor,
            evm_spec,
        }
    }

    /// Returns the EE account ID in the OL chain.
    pub fn strata_exec_account_id(&self) -> AccountId {
        self.strata_exec_account_id
    }

    /// Returns the bridge denomination and withdrawal policy.
    pub fn bridge_params(&self) -> &BridgeParams {
        &self.bridge_params
    }

    /// Returns the DA stream identity.
    pub fn blob_spec(&self) -> BlobSpec {
        self.blob_spec
    }

    /// Returns the Alpen spec activation schedule.
    pub fn spec_schedule(&self) -> &AlpenSpecSchedule {
        &self.spec_schedule
    }

    /// Returns the consensus minimum EIP-1559 base fee, in wei.
    pub fn base_fee_floor(&self) -> u64 {
        self.base_fee_floor
    }

    /// Returns the embedded EVM chain spec.
    pub fn evm_spec(&self) -> &EvmSpec {
        &self.evm_spec
    }

    /// Returns the derived reth chain spec of `version`.
    pub fn chain_spec(&self, version: AlpenSpecId) -> &Arc<ChainSpec> {
        self.evm_spec.chain_spec(version)
    }

    /// Returns the derived execution genesis block facts.
    pub fn genesis_block_info(&self) -> AlpenEeGenesisBlockInfo {
        self.evm_spec.genesis_info()
    }
}

// Manual instead of derived: `BridgeParams` has no `Default` (denomination
// zero is invalid), and `evm_spec` needs to go through `EvmSpec::default()`
// (an empty genesis document, i.e. what reth derives from `{}`) rather than a
// derived `Default` bound on `EvmSpec` itself.
impl Default for AlpenParams {
    /// Placeholder params for tests and benchmarks that construct an
    /// `AlpenParams` but don't exercise EVM execution: the default EE
    /// account id and bridge params, the canonical `ALPN` DA magic, the
    /// genesis spec schedule, and an empty EVM genesis. Not valid params for
    /// any real network.
    fn default() -> Self {
        Self {
            strata_exec_account_id: DEFAULT_ALPEN_EE_ACCOUNT_ID,
            bridge_params: BridgeParams::new_with_descriptor_limit(
                100_000_000,
                Some(1_000_000_000),
                81,
            )
            .expect("valid bridge params"),
            blob_spec: BlobSpec::new(MagicBytes::new(*b"ALPN")),
            spec_schedule: AlpenSpecSchedule::genesis(),
            base_fee_floor: DEFAULT_BASE_FEE_FLOOR,
            evm_spec: EvmSpec::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alpen_chainspec::DEV_CHAIN_SPEC;
    use serde_json::{json, Value};
    use strata_bridge_params::BridgeParams;
    use strata_l1_txfmt::MagicBytes;

    use super::{AlpenParams, DEFAULT_ALPEN_EE_ACCOUNT_ID, DEFAULT_BASE_FEE_FLOOR};
    use crate::{AlpenSpecSchedule, BlobSpec, EvmSpec};

    fn sample_params() -> AlpenParams {
        let evm_spec: EvmSpec =
            serde_json::from_str(DEV_CHAIN_SPEC).expect("dev chain should parse");
        AlpenParams::new(
            DEFAULT_ALPEN_EE_ACCOUNT_ID,
            BridgeParams::new_with_descriptor_limit(100_000_000, Some(1_000_000_000), 81)
                .expect("valid bridge params"),
            BlobSpec::new(MagicBytes::new(*b"ALPN")),
            AlpenSpecSchedule::genesis(),
            DEFAULT_BASE_FEE_FLOOR,
            evm_spec,
        )
    }

    fn sample_json() -> Value {
        serde_json::to_value(sample_params()).expect("params should serialize")
    }

    #[test]
    fn json_roundtrip_preserves_params() {
        let params = sample_params();

        let json = serde_json::to_string_pretty(&params).expect("params should serialize");
        let decoded: AlpenParams = serde_json::from_str(&json).expect("params should deserialize");

        assert_eq!(decoded, params);
    }

    #[test]
    fn default_round_trips_through_json() {
        let params = AlpenParams::default();

        let json = serde_json::to_string(&params).expect("params should serialize");
        let decoded: AlpenParams = serde_json::from_str(&json).expect("params should deserialize");

        assert_eq!(decoded, params);
    }

    #[test]
    fn json_defaults_missing_spec_schedule_to_genesis() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .remove("spec_schedule")
            .expect("spec_schedule should be present");

        let decoded: AlpenParams = serde_json::from_value(json).expect("params should deserialize");
        assert_eq!(decoded.spec_schedule(), &AlpenSpecSchedule::genesis());
    }

    #[test]
    fn json_defaults_missing_base_fee_floor_to_original_value() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .remove("base_fee_floor")
            .expect("base_fee_floor should be present");

        let decoded: AlpenParams = serde_json::from_value(json).expect("params should deserialize");
        assert_eq!(decoded.base_fee_floor(), DEFAULT_BASE_FEE_FLOOR);
    }

    #[test]
    fn json_preserves_custom_base_fee_floor() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .insert("base_fee_floor".to_owned(), json!(7));

        let decoded: AlpenParams = serde_json::from_value(json).expect("params should deserialize");
        assert_eq!(decoded.base_fee_floor(), 7);
    }

    #[test]
    fn json_rejects_missing_bridge_params() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .remove("bridge_params")
            .expect("bridge_params should be present");

        assert!(serde_json::from_value::<AlpenParams>(json).is_err());
    }

    #[test]
    fn json_rejects_unknown_fields() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .insert("genesis_blockhash".to_owned(), json!("0xdeadbeef"));

        assert!(serde_json::from_value::<AlpenParams>(json).is_err());
    }

    #[test]
    fn json_rejects_malformed_account_id() {
        let mut json = sample_json();
        json.as_object_mut()
            .expect("params should be an object")
            .insert("strata_exec_account_id".to_owned(), json!("01"));

        assert!(serde_json::from_value::<AlpenParams>(json).is_err());
    }
}
