//! Consolidated Alpen chain parameters.
//!
//! Defines the single top-level params artifact that describes how an Alpen
//! EE node interprets the chain: the EE account identity, bridge economics,
//! DA stream identity, fee policy, and the embedded EVM chain spec. It replaces the
//! previously fragmented loaders (`--ee-params` JSON, `--custom-chain`
//! genesis JSON, and DA-related CLI flags) with one validate-on-decode JSON
//! document.

mod blob_spec;
mod evm_spec;
mod extra_data;
mod genesis_info;
mod params;
mod spec_activations;

pub use blob_spec::BlobSpec;
pub use evm_spec::EvmSpec;
pub use extra_data::{
    header_spec_version, peek_spec_version, spec_version_for_block, HeaderExtra, HeaderExtraError,
};
pub use genesis_info::{ee_genesis_block_info, AlpenEeGenesisBlockInfo};
pub use params::{AlpenParams, DEFAULT_ALPEN_EE_ACCOUNT_ID, DEFAULT_BASE_FEE_FLOOR};
pub use spec_activations::{AlpenSpecId, AlpenSpecSchedule, AlpenSpecScheduleError};
