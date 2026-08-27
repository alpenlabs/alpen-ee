use alpen_ee_common::{ExecBlockPayload, ExecBlockRecord};
use alpen_ee_params::{AlpenParams, AlpenSpecId};
use strata_acct_types::Hash;
use strata_ee_acct_types::EeAccountState;
use strata_ee_chain_types::{ExecBlockCommitment, ExecBlockPackage, ExecInputs, ExecOutputs};
use strata_identifiers::{Buf32, OLBlockCommitment};

pub fn build_genesis_ee_account_state(params: &AlpenParams) -> EeAccountState {
    let genesis_info = params.genesis_block_info();
    EeAccountState::new(
        genesis_info.blockhash().0.into(),
        genesis_info.stateroot().0.into(),
        Vec::new(),
        Vec::new(),
    )
}

pub fn build_genesis_exec_block_package(params: &AlpenParams) -> ExecBlockPackage {
    // genesis_raw_block_encoded_hash: We dont really care about this for genesis block.
    // Sufficient for it to be deterministic.
    // Can be added to [`AlpenParams`] if correct value is required.
    let genesis_raw_block_encoded_hash = Hash::new([0; 32]);

    ExecBlockPackage::new(
        ExecBlockCommitment::new(
            params.genesis_block_info().blockhash().0.into(),
            genesis_raw_block_encoded_hash,
        ),
        ExecInputs::new_empty(),
        ExecOutputs::new_empty(),
    )
}

pub fn build_genesis_exec_block(
    params: &AlpenParams,
    genesis_ol_block: OLBlockCommitment,
) -> (ExecBlockRecord, ExecBlockPayload) {
    let genesis_package = build_genesis_exec_block_package(params);
    let genesis_account_state = build_genesis_ee_account_state(params);

    // These fields are for evm genesis block.
    let genesis_blocknum = params.genesis_block_info().blocknum();
    // Note: This timestamp is only used during blockproduction, so its not necessary for this to be
    // accurate. Can be added to [`AlpenParams`] if correct value is required.
    let genesis_block_timestamp_ms = 0;
    let genesis_parent_blockhash = Buf32([0; 32]); // 0x0
    let genesis_next_inbox_msg_idx = 0;
    let genesis_next_deposit_idx = 0;
    let genesis_next_spec_version = AlpenSpecId::V0;
    let genesis_messages = vec![];

    let block = ExecBlockRecord::new(
        genesis_package,
        genesis_account_state,
        genesis_blocknum,
        genesis_ol_block,
        genesis_block_timestamp_ms,
        genesis_parent_blockhash,
        genesis_next_inbox_msg_idx,
        genesis_next_deposit_idx,
        genesis_next_spec_version,
        genesis_messages,
    );
    let payload = ExecBlockPayload::from_bytes(Vec::new());

    (block, payload)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use alpen_ee_params::{AlpenParams, AlpenSpecId};
    use strata_acct_types::tree_hash::{Sha256Hasher, TreeHash};

    use super::build_genesis_ee_account_state;

    /// Genesis inner-state root of the EE account for each shipped chain spec.
    ///
    /// The functional tests pre-register the EE account in OL genesis and have
    /// to write these roots into the genesis-accounts file by hand, since
    /// computing one means SSZ-hashing [`EeAccountState`]. Pinning them here
    /// means a change to EE genesis fails this test instead of surfacing as an
    /// unexplained proof mismatch on the first update.
    ///
    /// Keep in sync with `GENESIS_INNER_STATE_ROOTS` in
    /// `functional-tests/common/datatool.py`.
    const GENESIS_INNER_STATE_ROOTS: &[(&str, &str)] = &[
        (
            "alpen-dev-chain",
            "a0a5f13344251d480f42dc85cabe0ca6dffa168e67ad32a9224970383baa63be",
        ),
        (
            "alpen-eest-chain",
            "7ec9df7c5a5d8177672e9e6e498353f92cd9e30ca902ab9bb989557675166058",
        ),
        (
            "devnet-chain",
            "185eea4e22a815a87a512843c279e42f87f9b57432d29abfe35b4ccfc0da1a1e",
        ),
        (
            "testnet-chain",
            "2a82d8daab762ffd91786783f47ca123d7d2206982533748697413e21c05f4b2",
        ),
        (
            "testnet3-chain",
            "87da9f8fd94022e63d24f05207dffd8a513136d1b07d68c0a350c47190085036",
        ),
    ];

    /// Builds params carrying `chain`'s genesis document. Only the EVM genesis
    /// feeds the inner-state root, so the other fields are the defaults.
    fn params_for_chain(chain: &str) -> AlpenParams {
        let spec = fs::read_to_string(format!(
            "{}/../../reth/chainspec/src/res/{chain}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("chain spec should be readable");

        serde_json::from_str(&format!(
            r#"{{"strata_exec_account_id":"{id}","bridge_params":{{"denomination":100000000,"max_withdrawal_amount":1000000000,"max_withdrawal_descriptor_len":81}},"blob_spec":{{"magic_bytes":"ALPN"}},"spec_schedule":{{"v0":0}},"evm_spec":{spec}}}"#,
            id = "01".repeat(32),
        ))
        .expect("params should parse")
    }

    #[test]
    fn genesis_inner_state_roots_are_stable() {
        for (chain, expected) in GENESIS_INNER_STATE_ROOTS {
            let state = build_genesis_ee_account_state(&params_for_chain(chain));
            let root = TreeHash::tree_hash_root::<Sha256Hasher>(&state);
            assert_eq!(
                hex::encode(root.0),
                *expected,
                "genesis inner state root changed for {chain}; update \
                 GENESIS_INNER_STATE_ROOTS here and in functional-tests/common/datatool.py"
            );
        }
    }

    #[test]
    fn eest_genesis_starts_at_seven_wei() {
        let params = params_for_chain("alpen-eest-chain");
        let genesis = params.chain_spec(AlpenSpecId::V0).genesis_header();

        assert_eq!(genesis.base_fee_per_gas, Some(7));
    }
}
