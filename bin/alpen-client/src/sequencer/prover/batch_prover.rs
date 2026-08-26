//! [`BatchProver`] impl that drives the chunk + acct paas provers.
//!
//! `request_proof_generation(batch_id)` reads the batch's chunk-id list
//! from `ChunkStorage::get_batch_chunks` and submits one `ChunkTask` per
//! chunk + one `BatchTask(batch_id)`. Both submits are idempotent;
//! multi-batch concurrency is paas-native.
//!
//! `check_proof_status(batch_id)` peeks the typed
//! [`EeBatchProofDbManager`] first (proof present → `Ready`); on miss
//! it maps `acct_handle.get_status(BatchTask)` to
//! [`ProofGenerationStatus`].
//!
//! Both route through [`PaasBatchProver::program_for`] first: a batch never
//! straddles a VK rotation (the sequencer force-seals right after any
//! rotation-consuming block), so each batch has exactly one governing
//! `AlpenSpecId`, stamped on it at seal time and read back here via
//! `batch_storage` — that version picks which resident [`ProverProgram`]
//! actually proves it.

use std::{collections::BTreeMap, sync::Arc};

use alpen_ee_common::{
    BatchId, BatchProver, BatchStorage, ChunkStatus, ChunkStorage, Proof, ProofGenerationStatus,
    ProofId,
};
use alpen_ee_params::AlpenSpecId;
use async_trait::async_trait;
use strata_paas::{ProverError as PaasError, ProverHandle, TaskStatus};
use tracing::{debug, info, warn};

use super::{
    spec_acct::AcctSpec,
    spec_chunk::ChunkSpec,
    spec_v0::{AcctSpecV0, ChunkSpecV0},
    BatchTask, ChunkTask, EeBatchProofDbManager,
};

/// One resident `--prover-program` candidate's launched chunk + acct prover
/// handles. Named to mirror the config-side `ProverProgramPaths` it was
/// resolved from.
pub(crate) enum ProverProgram {
    /// The frozen v0 pair. Split out because v0's guest reads a different
    /// input encoding, which needs its own [`ProofSpec`] pair and therefore
    /// its own handle types — see `super::spec_v0`.
    V0 {
        chunk_handle: ProverHandle<ChunkSpecV0>,
        acct_handle: ProverHandle<AcctSpecV0>,
    },
    /// The current pair, serving v1 onward.
    Current {
        chunk_handle: ProverHandle<ChunkSpec>,
        acct_handle: ProverHandle<AcctSpec>,
    },
}

impl ProverProgram {
    async fn submit_chunk(&self, task: ChunkTask) -> Result<(), PaasError> {
        match self {
            Self::V0 { chunk_handle, .. } => chunk_handle.submit(task).await,
            Self::Current { chunk_handle, .. } => chunk_handle.submit(task).await,
        }
    }

    async fn submit_batch(&self, task: BatchTask) -> Result<(), PaasError> {
        match self {
            Self::V0 { acct_handle, .. } => acct_handle.submit(task).await,
            Self::Current { acct_handle, .. } => acct_handle.submit(task).await,
        }
    }

    fn acct_status(&self, task: &BatchTask) -> Result<TaskStatus, PaasError> {
        match self {
            Self::V0 { acct_handle, .. } => acct_handle.get_status(task),
            Self::Current { acct_handle, .. } => acct_handle.get_status(task),
        }
    }
}

/// New-paas-backed [`BatchProver`].
///
/// Holds every resident [`ProverProgram`], keyed by the `AlpenSpecId` its
/// candidate declared. See the module doc for how a batch's own version
/// picks which one proves it.
pub(crate) struct PaasBatchProver {
    programs: BTreeMap<AlpenSpecId, ProverProgram>,
    chunk_storage: Arc<dyn ChunkStorage>,
    batch_storage: Arc<dyn BatchStorage>,
    batch_proofs: Arc<EeBatchProofDbManager>,
}

impl PaasBatchProver {
    pub(crate) fn new(
        programs: BTreeMap<AlpenSpecId, ProverProgram>,
        chunk_storage: Arc<dyn ChunkStorage>,
        batch_storage: Arc<dyn BatchStorage>,
        batch_proofs: Arc<EeBatchProofDbManager>,
    ) -> Self {
        Self {
            programs,
            chunk_storage,
            batch_storage,
            batch_proofs,
        }
    }

    /// Resolves which resident program proves `batch_id`, by the spec
    /// version stamped on it at seal time.
    async fn program_for(&self, batch_id: BatchId) -> eyre::Result<&ProverProgram> {
        let (batch, _status) = self
            .batch_storage
            .get_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| eyre::eyre!("no batch stored for {batch_id}"))?;
        let spec_version = batch.spec_version();
        self.programs.get(&spec_version).ok_or_else(|| {
            eyre::eyre!(
                "no resident prover program for spec version {spec_version} (batch {batch_id}); \
                 resident versions: {:?}",
                self.programs.keys().collect::<Vec<_>>()
            )
        })
    }
}

#[async_trait]
impl BatchProver for PaasBatchProver {
    async fn request_proof_generation(&self, batch_id: BatchId) -> eyre::Result<()> {
        let program = self.program_for(batch_id).await?;

        let chunks = self
            .chunk_storage
            .get_batch_chunks(batch_id)
            .await?
            .ok_or_else(|| eyre::eyre!("no chunks set for batch {batch_id}"))?;

        info!(
            %batch_id,
            chunk_count = chunks.len(),
            "submitting chunk + acct proof tasks"
        );

        for chunk_id in chunks {
            let task = ChunkTask(chunk_id);
            program
                .submit_chunk(task)
                .await
                .map_err(|e| eyre::eyre!("submit chunk task {chunk_id:?}: {e}"))?;

            let Some((_chunk, status)) = self.chunk_storage.get_chunk_by_id(chunk_id).await? else {
                warn!(?chunk_id, %batch_id, "submitted chunk task for missing chunk");
                continue;
            };

            if !matches!(status, ChunkStatus::ProofReady(_)) {
                self.chunk_storage
                    .update_chunk_status(chunk_id, ChunkStatus::ProofPending(task.to_string()))
                    .await?;
            }
        }

        program
            .submit_batch(BatchTask(batch_id))
            .await
            .map_err(|e| eyre::eyre!("submit acct task {batch_id}: {e}"))?;

        Ok(())
    }

    async fn check_proof_status(&self, batch_id: BatchId) -> eyre::Result<ProofGenerationStatus> {
        // Source of truth: the typed batch proof DB (the acct hook writes
        // there). Present ⇒ Ready.
        if self.batch_proofs.has_proof(batch_id) {
            return Ok(ProofGenerationStatus::Ready {
                proof_id: EeBatchProofDbManager::proof_id_for(batch_id),
            });
        }

        let program = self.program_for(batch_id).await?;

        // Else map paas's task lifecycle status. `TaskNotFound` ⇒ NotStarted
        // (we never submitted, or we're in a fresh process and haven't yet
        // recovered).
        match program.acct_status(&BatchTask(batch_id)) {
            Ok(TaskStatus::Completed) => {
                // Completed but not in the proof DB? Hook hasn't fired yet
                // or the DB lost its entry. Treat as Pending so the
                // lifecycle keeps polling.
                debug!(%batch_id, "acct task Completed but proof not yet in DB; reporting Pending");
                Ok(ProofGenerationStatus::Pending)
            }
            Ok(TaskStatus::PermanentFailure { error }) => {
                Ok(ProofGenerationStatus::Failed { reason: error })
            }
            Ok(TaskStatus::Pending)
            | Ok(TaskStatus::Proving { .. })
            | Ok(TaskStatus::Blocked { .. })
            | Ok(TaskStatus::TransientFailure { .. }) => Ok(ProofGenerationStatus::Pending),
            Err(PaasError::TaskNotFound(_)) => Ok(ProofGenerationStatus::NotStarted),
            Err(e) => {
                warn!(%batch_id, %e, "acct_handle.get_status failed");
                Err(eyre::eyre!("get_status({batch_id}): {e}"))
            }
        }
    }

    async fn get_proof(&self, proof_id: ProofId) -> eyre::Result<Option<Proof>> {
        Ok(self.batch_proofs.get_proof_by_id(proof_id))
    }
}
