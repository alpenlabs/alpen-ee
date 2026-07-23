use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use strata_acct_types::AccountId;
use strata_predicate::PredicateKey;

/// Default Alpen EE account id registered in generated OL params.
pub const DEFAULT_ALPEN_EE_ACCOUNT_ID: AccountId = AccountId::new([1u8; 32]);

/// Chain specific config, that needs to remain constant on all nodes
/// to ensure all stay on the same chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlpenEeParams {
    /// Account id of current EE in OL
    account_id: AccountId,

    /// Genesis blockhash of execution chain
    genesis_blockhash: B256,

    /// Genesis stateroot of execution chain
    genesis_stateroot: B256,

    /// Block number of execution chain genesis block
    /// This can potentially be non-zero, but is very unlikely.
    genesis_blocknum: u64,

    /// The account's update predicate key (VK) at genesis, before any
    /// rotation.
    genesis_update_vk: PredicateKey,
}

impl AlpenEeParams {
    /// Creates new chain parameters.
    pub fn new(
        account_id: AccountId,
        genesis_blockhash: B256,
        genesis_stateroot: B256,
        genesis_blocknum: u64,
        genesis_update_vk: PredicateKey,
    ) -> Self {
        Self {
            account_id,
            genesis_blockhash,
            genesis_stateroot,
            genesis_blocknum,
            genesis_update_vk,
        }
    }

    /// Parses chain parameters from a JSON string.
    pub fn from_json_str(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Serializes chain parameters to pretty-printed JSON.
    pub fn to_json_string_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Returns the EE account ID in the OL chain.
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    /// Returns the genesis block hash of the execution chain.
    pub fn genesis_blockhash(&self) -> B256 {
        self.genesis_blockhash
    }

    /// Returns the genesis state root of the execution chain.
    pub fn genesis_stateroot(&self) -> B256 {
        self.genesis_stateroot
    }

    /// Returns the genesis block number of the execution chain.
    pub fn genesis_blocknum(&self) -> u64 {
        self.genesis_blocknum
    }

    /// Returns the account's genesis update predicate key.
    pub fn genesis_update_vk(&self) -> &PredicateKey {
        &self.genesis_update_vk
    }
}

#[cfg(test)]
mod tests {
    use strata_predicate::PredicateKey;

    use super::{AlpenEeParams, DEFAULT_ALPEN_EE_ACCOUNT_ID};

    #[test]
    fn json_roundtrip_preserves_params() {
        let params = AlpenEeParams::new(
            DEFAULT_ALPEN_EE_ACCOUNT_ID,
            [2u8; 32].into(),
            [3u8; 32].into(),
            42,
            PredicateKey::always_accept(),
        );

        let json = params
            .to_json_string_pretty()
            .expect("params should serialize");
        let decoded = AlpenEeParams::from_json_str(&json).expect("params should deserialize");

        assert_eq!(decoded, params);
    }

    #[test]
    fn json_rejects_malformed_account_id() {
        let json = r#"{
            "account_id": "01",
            "genesis_blockhash": "0x0202020202020202020202020202020202020202020202020202020202020202",
            "genesis_stateroot": "0x0303030303030303030303030303030303030303030303030303030303030303",
            "genesis_blocknum": 0,
            "genesis_update_vk": "AlwaysAccept"
        }"#;

        assert!(AlpenEeParams::from_json_str(json).is_err());
    }
}
