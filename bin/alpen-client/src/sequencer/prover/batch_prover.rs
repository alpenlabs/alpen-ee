//! [`BatchProver`] impl that drives the chunk + acct paas provers.
//!
//! `request_proof_generation(batch_id)` reads the batch's chunk-id list
//! from `ChunkStorage::get_batch_chunks` and submits one `ChunkTask` per
//! chunk + one `BatchTask(batch_id)`. Both submits are idempotent;
//! multi-batch concurrency is paas-native.
//!
//! `check_proof_status(batch_id)` peeks the typed
//! [`EeBatchProofDbManager`] first (proof present → `Ready`); on miss
//! it maps the acct prover's `get_status(BatchTask)` to
//! [`ProofGenerationStatus`].
//!
//! Both route through [`PaasBatchProver::program_for`] first. When the
//! batch's spec version has one resident [`ProverProgram`], that program
//! proves it. A version has several while it rotates to a new key, and then
//! the batch goes to the one whose key the OL will check its update against.
//! A batch never straddles a VK rotation (the sequencer force-seals right
//! after any rotation-consuming block), so that is one key, which
//! [`update_key_for_batch`] works out.

use std::sync::Arc;

use alpen_common::{
    BatchId, BatchProver, ChunkStatus, Proof, ProofGenerationStatus, ProofId, SequencerOLClient,
};
use alpen_sequencer::{update_key_for_batch, ProgramRegistry};
use alpen_storage_api::{BatchStorage, ChunkStorage, ExecBlockStorage};
use alpen_storage_store::ProgramScope;
use async_trait::async_trait;
use metrics::counter;
use strata_paas::{ProverError as PaasError, ProverHandle, TaskStatus};
use tracing::{debug, info, warn};

use super::{
    spec_acct::AcctSpec, spec_chunk::ChunkSpec, BatchTask, ChunkTask, EeBatchProofDbManager,
};
use crate::metrics::{PROOF_STATUS_CHECKS, PROOF_TASKS_SUBMITTED};

/// One resident `[[sequencer.prover.programs]]` entry's chunk + acct provers.
/// Named to mirror the config-side `ProverProgramConfig` it was resolved from.
///
/// `C` and `A` are what the program holds for its chunk and acct provers.
/// Startup builds the provers first and launches them after, so a program
/// holds built provers until then. The defaults are the launched handles,
/// which [`PaasBatchProver`] routes batches to.
pub(crate) struct ProverProgram<C = ProverHandle<ChunkSpec>, A = ProverHandle<AcctSpec>> {
    pub(crate) scope: ProgramScope,
    pub(crate) chunk: C,
    pub(crate) acct: A,
}

/// Where [`PaasBatchProver`] reads what it needs to prove a batch and to pick
/// its program.
pub(crate) struct BatchProverSources {
    pub(crate) ol_client: Arc<dyn SequencerOLClient + Send + Sync>,
    pub(crate) chunk_storage: Arc<dyn ChunkStorage>,
    pub(crate) batch_storage: Arc<dyn BatchStorage>,
    pub(crate) exec_storage: Arc<dyn ExecBlockStorage>,
    pub(crate) batch_proofs: Arc<EeBatchProofDbManager>,
}

/// New-paas-backed [`BatchProver`].
///
/// Holds every resident [`ProverProgram`]. See the module doc for how a
/// batch picks which one proves it.
pub(crate) struct PaasBatchProver {
    programs: Vec<ProverProgram>,
    ol_client: Arc<dyn SequencerOLClient + Send + Sync>,
    chunk_storage: Arc<dyn ChunkStorage>,
    batch_storage: Arc<dyn BatchStorage>,
    exec_storage: Arc<dyn ExecBlockStorage>,
    batch_proofs: Arc<EeBatchProofDbManager>,
}

impl PaasBatchProver {
    pub(crate) fn new(programs: Vec<ProverProgram>, sources: BatchProverSources) -> Self {
        let BatchProverSources {
            ol_client,
            chunk_storage,
            batch_storage,
            exec_storage,
            batch_proofs,
        } = sources;
        Self {
            programs,
            ol_client,
            chunk_storage,
            batch_storage,
            exec_storage,
            batch_proofs,
        }
    }

    /// The key and spec version of each resident program, for the block
    /// builder to resolve rotations with. Built from the programs this prover
    /// holds, so the block builder only honors a rotation that it can prove.
    pub(crate) fn program_registry(&self) -> ProgramRegistry {
        ProgramRegistry::new(
            self.programs
                .iter()
                .map(|program| (program.scope.key().clone(), program.scope.spec_version())),
        )
        .expect("launch rejects two programs with the same key")
    }

    /// Resolves which resident program proves `batch_id`: the one whose key
    /// the OL will check the batch's update against.
    async fn program_for(&self, batch_id: BatchId) -> eyre::Result<&ProverProgram> {
        let (batch, _status) = self
            .batch_storage
            .get_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| eyre::eyre!("no batch stored for {batch_id}"))?;
        let spec_version = batch.spec_version();

        // The OL only accepts a proof made with the key it checks the batch
        // against. If one program proves the version, it has to be that key,
        // so working the key out would pick the same program.
        let mut of_version = self
            .programs
            .iter()
            .filter(|program| program.scope.spec_version() == spec_version);
        match (of_version.next(), of_version.next()) {
            (None, _) => {
                return Err(eyre::eyre!(
                    "no resident prover program for spec version {spec_version} (batch \
                     {batch_id})"
                ))
            }
            (Some(only), None) => return Ok(only),
            (Some(_), Some(_)) => {}
        }

        let key = update_key_for_batch(
            self.ol_client.as_ref(),
            self.batch_storage.as_ref(),
            self.exec_storage.as_ref(),
            &batch,
        )
        .await?;

        let program = self
            .programs
            .iter()
            .find(|program| *program.scope.key() == key)
            .ok_or_else(|| {
                eyre::eyre!(
                    "no resident prover program has the key {key} the OL checks batch \
                     {batch_id} against"
                )
            })?;
        // Only the config entry says which version a program proves, so a
        // mislabeled entry shows up here.
        if program.scope.spec_version() != spec_version {
            return Err(eyre::eyre!(
                "batch {batch_id} runs under {spec_version}, but the program with its key \
                 {key} is configured for {}",
                program.scope.spec_version()
            ));
        }

        Ok(program)
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
                .chunk
                .submit(task)
                .await
                .map_err(|e| eyre::eyre!("submit chunk task {chunk_id:?}: {e}"))?;
            counter!(PROOF_TASKS_SUBMITTED, "kind" => "chunk").increment(1);

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
            .acct
            .submit(BatchTask(batch_id))
            .await
            .map_err(|e| eyre::eyre!("submit acct task {batch_id}: {e}"))?;
        counter!(PROOF_TASKS_SUBMITTED, "kind" => "acct").increment(1);

        Ok(())
    }

    async fn check_proof_status(&self, batch_id: BatchId) -> eyre::Result<ProofGenerationStatus> {
        // Source of truth: the typed batch proof DB (the acct hook writes
        // there). Present ⇒ Ready.
        if self.batch_proofs.has_proof(batch_id) {
            counter!(PROOF_STATUS_CHECKS, "status" => "ready").increment(1);
            return Ok(ProofGenerationStatus::Ready {
                proof_id: EeBatchProofDbManager::proof_id_for(batch_id),
            });
        }

        let program = self.program_for(batch_id).await?;

        // Else map paas's task lifecycle status. `TaskNotFound` ⇒ NotStarted
        // (we never submitted, or we're in a fresh process and haven't yet
        // recovered).
        let status = program.acct.get_status(&BatchTask(batch_id));
        let (status_name, result) = match status {
            Ok(TaskStatus::Completed) => {
                // Completed but not in the proof DB? Hook hasn't fired yet
                // or the DB lost its entry. Treat as Pending so the
                // lifecycle keeps polling.
                debug!(%batch_id, "acct task Completed but proof not yet in DB; reporting Pending");
                ("completed", Ok(ProofGenerationStatus::Pending))
            }
            Ok(TaskStatus::PermanentFailure { error }) => (
                "failed",
                Ok(ProofGenerationStatus::Failed { reason: error }),
            ),
            Ok(TaskStatus::Pending) => ("pending", Ok(ProofGenerationStatus::Pending)),
            Ok(TaskStatus::Proving { .. }) => ("proving", Ok(ProofGenerationStatus::Pending)),
            Ok(TaskStatus::Blocked { .. }) => ("blocked", Ok(ProofGenerationStatus::Pending)),
            Ok(TaskStatus::TransientFailure { .. }) => {
                ("transient_failure", Ok(ProofGenerationStatus::Pending))
            }
            Err(PaasError::TaskNotFound(_)) => {
                ("not_started", Ok(ProofGenerationStatus::NotStarted))
            }
            Err(e) => {
                warn!(%batch_id, %e, "acct prover get_status failed");
                ("error", Err(eyre::eyre!("get_status({batch_id}): {e}")))
            }
        };
        counter!(PROOF_STATUS_CHECKS, "status" => status_name).increment(1);
        result
    }

    async fn get_proof(&self, proof_id: ProofId) -> eyre::Result<Option<Proof>> {
        Ok(self.batch_proofs.get_proof_by_id(proof_id))
    }
}
