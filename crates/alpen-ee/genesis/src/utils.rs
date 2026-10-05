use alpen_ee_common::{ExecBlockPayload, ExecBlockRecord};
use alpen_ee_params::AlpenParams;
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
    // The version the first non-genesis block builds under. A chain that
    // launches on a newer version schedules it at coordinate 0, so it starts
    // there rather than replaying the upgrades that predate its launch.
    let genesis_next_spec_version = params.spec_schedule().active_at(0);
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
    use std::{fs, path::Path};

    use alpen_ee_params::AlpenParams;
    use strata_acct_types::tree_hash::{Sha256Hasher, TreeHash};

    use super::build_genesis_ee_account_state;

    /// Genesis inner-state root of the EE account for each `params/<network>.json`,
    /// sorted by network.
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
            "dev",
            "a0a5f13344251d480f42dc85cabe0ca6dffa168e67ad32a9224970383baa63be",
        ),
        (
            "mainnet",
            "308ce726a90fd45d3638fd86dec816cca262edc0d5acee9b130cfa33dbb740b0",
        ),
        (
            "staging",
            "2a82d8daab762ffd91786783f47ca123d7d2206982533748697413e21c05f4b2",
        ),
        (
            "testnet",
            "87da9f8fd94022e63d24f05207dffd8a513136d1b07d68c0a350c47190085036",
        ),
    ];

    /// Also parses every params file, so a bad one fails here instead of in a
    /// release build.
    #[test]
    fn genesis_inner_state_roots_are_stable() {
        let params_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../params");
        let mut networks: Vec<String> = fs::read_dir(&params_dir)
            .expect("params dir should be readable")
            .map(|entry| entry.expect("params dir entry should be readable").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .expect("params file name should be UTF-8")
                    .to_owned()
            })
            .collect();
        networks.sort();
        let pinned: Vec<&str> = GENESIS_INNER_STATE_ROOTS
            .iter()
            .map(|(network, _)| *network)
            .collect();
        assert_eq!(
            networks, pinned,
            "every params/<network>.json needs a root in GENESIS_INNER_STATE_ROOTS"
        );

        for (network, expected) in GENESIS_INNER_STATE_ROOTS {
            let json = fs::read_to_string(params_dir.join(format!("{network}.json")))
                .expect("params file should be readable");
            let params: AlpenParams = serde_json::from_str(&json)
                .unwrap_or_else(|err| panic!("params/{network}.json should parse: {err}"));
            let state = build_genesis_ee_account_state(&params);
            let root = TreeHash::tree_hash_root::<Sha256Hasher>(&state);
            assert_eq!(
                hex::encode(root.0),
                *expected,
                "genesis inner state root changed for {network}; update \
                 GENESIS_INNER_STATE_ROOTS here and in functional-tests/common/datatool.py"
            );
        }
    }
}
