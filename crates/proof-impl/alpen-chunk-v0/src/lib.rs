//! EE chunk proof implementation for the **v0** spec version, frozen to match the deployed v0
//! guest.
//!
//! This is a port of the v0-era `strata-proofimpl-alpen-chunk`, kept because
//! the v0 program is already deployed: its ELF is fixed, so the host has to
//! speak the input encoding that binary reads, not the other way around. That
//! encoding differs from the current one -- v0 reads the genesis config and
//! bridge params as zkVM input, where later versions have them baked into the
//! guest at build time -- so it cannot be expressed as a parameter on the
//! current crate.
//!
//! Only the versions still in service need a crate here. Do not add features
//! or fix bugs in this one: changing it changes the verifying key, and the
//! deployed program's key is what OL checks proofs against. Current and future
//! versions live in `strata-proofimpl-alpen-chunk`.

use std::sync::Arc;

use alpen_reth_evm::evm::AlpenEvmFactory;
use reth_chainspec::ChainSpec;
use rkyv::rancor::Error as RkyvError;
use rsp_primitives::genesis::Genesis;
use ssz::Decode;
use strata_bridge_params::BridgeParams;
use strata_ee_chunk_runtime::ArchivedPrivateInput;
use strata_evm_ee::EvmExecutionEnvironment;
use zkaleido::ZkVmEnvSerde;

mod program;

pub use program::{EeChunkProgram, EeChunkProofInput};

/// Guest entry point for EE chunk proof generation.
///
/// Reads a genesis config and an rkyv-serialized private input from the zkVM,
/// verifies the chunk transition using the EVM execution environment, and
/// commits the resulting [`strata_ee_chain_types::ChunkTransition`] as SSZ
/// public output.
pub fn process_ee_chunk(zkvm: &impl ZkVmEnvSerde) {
    let genesis: Genesis = zkvm.read_serde();
    let chain_spec: Arc<ChainSpec> = Arc::new((&genesis).try_into().unwrap());

    let buf = zkvm.read_buf();
    let input: &ArchivedPrivateInput = rkyv::access::<ArchivedPrivateInput, RkyvError>(&buf)
        .expect("failed to access rkyv archive");

    let withdrawal_ssz = zkvm.read_buf();
    let bridge_params = BridgeParams::from_ssz_bytes(&withdrawal_ssz)
        .expect("failed to deserialize withdrawal params");
    let evm_factory = AlpenEvmFactory::from_bridge_params(&bridge_params);
    let ee = EvmExecutionEnvironment::new(chain_spec, evm_factory);

    strata_ee_chunk_runtime::verify_input(&ee, input).expect("chunk verification failed");

    zkvm.commit_buf(input.chunk_transition_ssz());
}
