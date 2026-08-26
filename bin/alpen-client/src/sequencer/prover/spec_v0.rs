//! Proof specs for the frozen **v0** programs.
//!
//! v0 is deployed, so its guest ELF is fixed and reads an input encoding the
//! current one no longer produces: the genesis config and bridge params come
//! in as zkVM input rather than baked into the guest at build time. That is a
//! different wire format, so it needs its own [`ProofSpec`] pair —
//! [`ProofSpec::Program`] binds one program type per spec.
//!
//! Only the encoding differs, not what goes into it, so each spec here wraps
//! its current-version counterpart and re-wraps what that spec's
//! `fetch_input` already assembled. Nothing about how inputs are gathered is
//! duplicated.

use alpen_ee_params::AlpenParams;
use async_trait::async_trait;
use rsp_primitives::genesis::Genesis;
use strata_bridge_params::BridgeParams;
use strata_paas::{InputResolution, ProofSpec, ProverResult};
use strata_proofimpl_alpen_acct_v0::{
    EeAcctProgram as EeAcctProgramV0, EeAcctProofInput as EeAcctProofInputV0,
};
use strata_proofimpl_alpen_chunk_v0::{
    EeChunkProgram as EeChunkProgramV0, EeChunkProofInput as EeChunkProofInputV0,
};

use super::{spec_acct::AcctSpec, spec_chunk::ChunkSpec, BatchTask, ChunkTask};

/// The v0 guest derives its chain spec from the genesis config it is handed,
/// so the host has to hand it the one v0's rules were defined by. v0's chain
/// spec is the genesis document as-is — no version delta applies at v0 —
/// which is exactly what `evm_spec().genesis()` carries.
fn v0_genesis(params: &AlpenParams) -> Genesis {
    Genesis::Custom(params.evm_spec().genesis().config.clone())
}

/// Chunk proof spec for the frozen v0 program.
#[derive(Clone)]
pub(crate) struct ChunkSpecV0 {
    inner: ChunkSpec,
    genesis: Genesis,
    bridge_params: BridgeParams,
}

impl ChunkSpecV0 {
    pub(crate) fn new(inner: ChunkSpec, params: &AlpenParams) -> Self {
        Self {
            inner,
            genesis: v0_genesis(params),
            bridge_params: *params.bridge_params(),
        }
    }
}

#[async_trait]
impl ProofSpec for ChunkSpecV0 {
    type Task = ChunkTask;
    type Program = EeChunkProgramV0;

    async fn resolve_input(
        &self,
        task: &Self::Task,
    ) -> ProverResult<InputResolution<EeChunkProofInputV0>> {
        let input = self
            .inner
            .fetch_input(task)
            .await
            .map(|current| EeChunkProofInputV0 {
                genesis: self.genesis.clone(),
                private_input: current.private_input,
                bridge_params: self.bridge_params,
            });
        InputResolution::from_result(input)
    }
}

/// Account-update proof spec for the frozen v0 program.
#[derive(Clone)]
pub(crate) struct AcctSpecV0 {
    inner: AcctSpec,
    genesis: Genesis,
    bridge_params: BridgeParams,
}

impl AcctSpecV0 {
    pub(crate) fn new(inner: AcctSpec, params: &AlpenParams) -> Self {
        Self {
            inner,
            genesis: v0_genesis(params),
            bridge_params: *params.bridge_params(),
        }
    }
}

#[async_trait]
impl ProofSpec for AcctSpecV0 {
    type Task = BatchTask;
    type Program = EeAcctProgramV0;

    async fn resolve_input(
        &self,
        task: &Self::Task,
    ) -> ProverResult<InputResolution<EeAcctProofInputV0>> {
        let input = self
            .inner
            .fetch_input(task)
            .await
            .map(|current| EeAcctProofInputV0 {
                genesis: self.genesis.clone(),
                ee_private_input: current.ee_private_input,
                snark_acct_private_input: current.snark_acct_private_input,
                da_witness: current.da_witness,
                bridge_params: self.bridge_params,
            });
        InputResolution::from_result(input)
    }
}
