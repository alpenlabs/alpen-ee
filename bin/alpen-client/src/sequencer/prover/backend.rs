//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (native for dev/test, SP1 remote otherwise), builds the
//! underlying paas provers, checks the resulting account predicate key
//! against the OL's expected `update_vk`, and launches both prover
//! services.

use std::{sync::Arc, time::Duration};

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
#[cfg(feature = "sp1")]
use strata_zkvm_hosts::sp1::{alpen_acct_host, alpen_chunk_host};
use tracing::info;
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

use super::{AcctSpec, ChunkSpec, EeBatchProofDbManager, PaasBatchProver};
use crate::service_executor::ServiceExecutor;

/// Default end-to-end deadline applied to the SP1 prover network for the EE
/// chunk + acct provers when `--sp1-proof-deadline-secs` is not set. Chosen
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
    Sp1 { deadline_secs: Option<u64> },
}

/// Picks a prover backend, builds the paas provers, validates the resulting
/// account predicate key against the OL's expected `update_vk`, and
/// launches both prover services.
pub(crate) async fn launch_validated_ee_batch_prover(
    ol_client: &(impl SequencerOLClient + Send + Sync),
    service_executor: &ServiceExecutor,
    builders: EeProverBuilders,
    stores: EeProverStores,
    use_native_prover: bool,
    sp1_deadline_secs: Option<u64>,
    params: Arc<AlpenParams>,
) -> eyre::Result<Arc<PaasBatchProver>> {
    let ol_account_update_vk = ol_client
        .get_latest_account_update_vk()
        .await
        .context("failed to fetch OL account update_vk for prover validation")?;
    let backend = if use_native_prover {
        EeProverBackend::Native
    } else {
        EeProverBackend::Sp1 {
            deadline_secs: sp1_deadline_secs,
        }
    };
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
        EeProverBackend::Sp1 { deadline_secs } => {
            use zkaleido::ZkVmExecutor;

            let deadline_secs = deadline_secs.unwrap_or(DEFAULT_SP1_DEADLINE_SECS);
            let deadline = Duration::from_secs(deadline_secs);
            info!(
                target: "alpen-client",
                deadline_secs,
                "sp1 EE prover deadline configured"
            );

            // TODO(STR-4155): `alpen_chunk_host`/`alpen_acct_host` resolve their ELF either
            // from a compile-time dependency on `strata-sp1-guest-builder` (one chainspec
            // baked in) or an `ELF_BASE_PATH` env var (see `strata_zkvm_hosts::sp1`). Take
            // the guest ELF paths as explicit CLI/config args to this binary instead, so one
            // `alpen-client` build can run against different guest ELFs without relying on a
            // rebuild.
            let sp1_config = SP1HostConfig::default().with_deadline(deadline);
            let chunk_host: SP1Host = (**alpen_chunk_host(sp1_config.clone()).await).clone();
            let acct_host: SP1Host = (**alpen_acct_host(sp1_config).await).clone();
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
            "remote SP1 prover is not compiled in; pass --dev-native-prover \
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
