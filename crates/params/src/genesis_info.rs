//! EE genesis block facts derived from a chain spec.

use alloy_primitives::B256;
use reth_chainspec::ChainSpec;

/// Genesis block data that must match the Alpen EE params file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlpenEeGenesisBlockInfo {
    blockhash: B256,
    stateroot: B256,
    blocknum: u64,
}

impl AlpenEeGenesisBlockInfo {
    /// Returns the execution genesis block hash.
    pub fn blockhash(&self) -> B256 {
        self.blockhash
    }

    /// Returns the execution genesis state root.
    pub fn stateroot(&self) -> B256 {
        self.stateroot
    }

    /// Returns the execution genesis block number.
    pub fn blocknum(&self) -> u64 {
        self.blocknum
    }
}

/// Extracts Alpen EE genesis block info from a chain spec.
pub fn ee_genesis_block_info(chain_spec: &ChainSpec) -> AlpenEeGenesisBlockInfo {
    let genesis_header = chain_spec.genesis_header();
    let genesis_stateroot = genesis_header.state_root;
    let genesis_hash = chain_spec.genesis_hash();
    let genesis_blocknum = genesis_header.number;

    AlpenEeGenesisBlockInfo {
        blockhash: genesis_hash,
        stateroot: genesis_stateroot,
        blocknum: genesis_blocknum,
    }
}

#[cfg(test)]
pub(crate) fn ee_genesis_block_info_from_json(
    chain_json: &str,
) -> serde_json::Result<AlpenEeGenesisBlockInfo> {
    use alloy_genesis::Genesis;

    let genesis: Genesis = serde_json::from_str(chain_json)?;
    let chain_spec = ChainSpec::from_genesis(genesis);

    Ok(ee_genesis_block_info(&chain_spec))
}
