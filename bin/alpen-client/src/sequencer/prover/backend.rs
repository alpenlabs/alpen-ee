//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (native for dev/test, SP1 remote otherwise), builds the
//! underlying paas provers, checks the resulting account predicate key
//! against the OL's expected `update_vk`, and launches both prover
//! services.

#[cfg(feature = "sp1")]
use std::fs;
use std::{path::PathBuf, sync::Arc, time::Duration};

use alpen_ee_common::{ChunkStorage, SequencerOLClient};
use alpen_ee_params::AlpenParams;
use eyre::Context;
use strata_paas::{Prover, ProverBuilder, ProverHandle, ProverServiceBuilder};
use strata_predicate::PredicateKey;
use strata_proofimpl_alpen_acct::EeAcctProgram;
use strata_proofimpl_alpen_chunk::EeChunkProgram;
use strata_proofimpl_predicate_keys::{
    validate_expected_predicate_key, NativeAlpenAcctPredicateKey, NativeAlpenChunkPredicateKey,
    PredicateKeyProvider, Sp1Groth16PredicateKey,
};
use tracing::info;
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

use super::{AcctSpec, ChunkSpec, EeBatchProofDbManager, PaasBatchProver};
use crate::service_executor::ServiceExecutor;

/// Default end-to-end deadline applied to the SP1 prover network for the EE
/// chunk + acct provers when `sequencer.sp1_proof_deadline_secs` is not set. Chosen
/// to comfortably cover chunk/acct proofs while still failing fast on stuck
/// requests.
#[cfg(feature = "sp1")]
const DEFAULT_SP1_DEADLINE_SECS: u64 = 4 * 60 * 60;

pub(crate) struct EeProverBuilders {
    pub(crate) chunk: ProverBuilder<ChunkSpec>,
    pub(crate) account: ProverBuilder<AcctSpec>,
}

pub(crate) struct EeProverStores {
    pub(crate) chunk_storage: Arc<dyn ChunkStorage>,
    pub(crate) batch_proofs: Arc<EeBatchProofDbManager>,
}

struct EeProverConfig {
    provers: EeProvers,
    account_predicate_key: PredicateKey,
}

struct EeProvers {
    chunk: Prover<ChunkSpec>,
    account: Prover<AcctSpec>,
}

enum EeProverBackend {
    Native,
    Sp1 {
        deadline_secs: Option<u64>,
        chunk_elf_path: PathBuf,
        acct_elf_path: PathBuf,
    },
}

/// Config-derived knobs that select and configure the EE batch prover backend.
pub(crate) struct EeProverBackendArgs {
    pub(crate) use_native_prover: bool,
    pub(crate) sp1_deadline_secs: Option<u64>,
    pub(crate) chunk_elf_path: Option<PathBuf>,
    pub(crate) acct_elf_path: Option<PathBuf>,
}

impl EeProverBackendArgs {
    fn into_backend(self) -> eyre::Result<EeProverBackend> {
        if self.use_native_prover {
            return Ok(EeProverBackend::Native);
        }
        let chunk_elf_path = self.chunk_elf_path.ok_or_else(|| {
            eyre::eyre!(
                "sequencer.chunk_elf_path is required unless sequencer.dev_native_prover is true"
            )
        })?;
        let acct_elf_path = self.acct_elf_path.ok_or_else(|| {
            eyre::eyre!(
                "sequencer.acct_elf_path is required unless sequencer.dev_native_prover is true"
            )
        })?;
        Ok(EeProverBackend::Sp1 {
            deadline_secs: self.sp1_deadline_secs,
            chunk_elf_path,
            acct_elf_path,
        })
    }
}

/// Picks a prover backend, builds the paas provers, validates the resulting
/// account predicate key against the OL's expected `update_vk`, and
/// launches both prover services.
pub(crate) async fn launch_validated_ee_batch_prover(
    ol_client: &(impl SequencerOLClient + Send + Sync),
    service_executor: &ServiceExecutor,
    builders: EeProverBuilders,
    stores: EeProverStores,
    backend_args: EeProverBackendArgs,
    params: Arc<AlpenParams>,
) -> eyre::Result<Arc<PaasBatchProver>> {
    let ol_account_update_vk = ol_client
        .get_latest_account_update_vk()
        .await
        .context("failed to fetch OL account update_vk for prover validation")?;
    let backend = backend_args.into_backend()?;
    let prover_config = build_ee_prover_config(builders, backend, params).await?;

    validate_ee_account_prover_predicate_key(
        &ol_account_update_vk,
        &prover_config.account_predicate_key,
    )?;

    let (chunk_handle, acct_handle) =
        launch_ee_prover_services(service_executor, prover_config.provers).await?;

    Ok(Arc::new(PaasBatchProver::new(
        chunk_handle,
        acct_handle,
        stores.chunk_storage,
        stores.batch_proofs,
    )))
}

async fn build_ee_prover_config(
    builders: EeProverBuilders,
    backend: EeProverBackend,
    params: Arc<AlpenParams>,
) -> eyre::Result<EeProverConfig> {
    match backend {
        EeProverBackend::Native => {
            info!(
                target: "alpen-client",
                "EE chunk + acct provers: native host (dev/test only)"
            );

            let chunk_program = EeChunkProgram::new((*params).clone());
            let chunk = builders.chunk.native(chunk_program.native_host());
            let chunk_predicate_key = NativeAlpenChunkPredicateKey
                .predicate_key()
                .expect("native chunk predicate key must be available");
            let acct_program = EeAcctProgram::new(chunk_predicate_key, (*params).clone());
            let account = builders.account.native(acct_program.native_host());
            let account_predicate_key = NativeAlpenAcctPredicateKey
                .predicate_key()
                .expect("native account predicate key must be available");

            Ok(EeProverConfig {
                provers: EeProvers { chunk, account },
                account_predicate_key,
            })
        }
        #[cfg(feature = "sp1")]
        EeProverBackend::Sp1 {
            deadline_secs,
            chunk_elf_path,
            acct_elf_path,
        } => {
            use zkaleido::ZkVmExecutor;

            let deadline_secs = deadline_secs.unwrap_or(DEFAULT_SP1_DEADLINE_SECS);
            let deadline = Duration::from_secs(deadline_secs);
            info!(
                target: "alpen-client",
                deadline_secs,
                ?chunk_elf_path,
                ?acct_elf_path,
                "sp1 EE prover deadline configured"
            );

            let sp1_config = SP1HostConfig::default().with_deadline(deadline);
            let chunk_elf = fs::read(&chunk_elf_path).with_context(|| {
                format!(
                    "failed to read chunk guest ELF at {}",
                    chunk_elf_path.display()
                )
            })?;
            let acct_elf = fs::read(&acct_elf_path).with_context(|| {
                format!(
                    "failed to read account guest ELF at {}",
                    acct_elf_path.display()
                )
            })?;
            let chunk_host = SP1Host::init_with_config(&chunk_elf, sp1_config.clone()).await;
            let acct_host = SP1Host::init_with_config(&acct_elf, sp1_config).await;
            let account_predicate_key = Sp1Groth16PredicateKey::new(acct_host.program_id().0)
                .predicate_key()
                .map_err(|e| {
                    eyre::eyre!("failed to derive local SP1 account prover predicate key: {e}")
                })?;

            Ok(EeProverConfig {
                provers: EeProvers {
                    chunk: builders.chunk.remote(chunk_host),
                    account: builders.account.remote(acct_host),
                },
                account_predicate_key,
            })
        }
        #[cfg(not(feature = "sp1"))]
        EeProverBackend::Sp1 { .. } => Err(eyre::eyre!(
            "remote SP1 prover is not compiled in; set sequencer.dev_native_prover = true \
             or build with the `sp1` feature"
        )),
    }
}

async fn launch_ee_prover_services(
    service_executor: &ServiceExecutor,
    provers: EeProvers,
) -> eyre::Result<(ProverHandle<ChunkSpec>, ProverHandle<AcctSpec>)> {
    let prover_tick = Duration::from_secs(5);
    let chunk_handle = ProverServiceBuilder::new(provers.chunk)
        .tick_interval(prover_tick)
        .launch(service_executor)
        .await
        .map_err(|e| eyre::eyre!("launching chunk prover service: {e}"))?;
    let acct_handle = ProverServiceBuilder::new(provers.account)
        .tick_interval(prover_tick)
        .launch(service_executor)
        .await
        .map_err(|e| eyre::eyre!("launching acct prover service: {e}"))?;

    Ok((chunk_handle, acct_handle))
}

fn validate_ee_account_prover_predicate_key(
    ol_update_vk: &PredicateKey,
    local_predicate_key: &PredicateKey,
) -> eyre::Result<()> {
    validate_expected_predicate_key(ol_update_vk, local_predicate_key).map_err(|e| {
        eyre::eyre!(
            "OL account update_vk does not match local EE account prover predicate key: {e}"
        )
    })
}

#[cfg(test)]
mod tests {
    use strata_predicate::PredicateTypeId;

    use super::*;

    #[test]
    fn ee_account_prover_predicate_key_validation_accepts_match() {
        let predicate = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![1, 2, 3]);

        validate_ee_account_prover_predicate_key(&predicate, &predicate).unwrap();
    }

    #[test]
    fn ee_account_prover_predicate_key_validation_rejects_mismatch() {
        let ol_update_vk = PredicateKey::new(PredicateTypeId::Bip340Schnorr, vec![1, 2, 3]);
        let local_predicate_key = PredicateKey::new(PredicateTypeId::Sp1Groth16, vec![4, 5, 6]);

        let err = validate_ee_account_prover_predicate_key(&ol_update_vk, &local_predicate_key)
            .unwrap_err()
            .to_string();

        assert!(err.contains("OL account update_vk does not match local EE account prover"));
        assert!(err.contains("predicate key mismatch"));
    }
}
