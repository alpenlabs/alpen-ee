//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (`--prover-backend native`, the default, or `sp1`), builds the
//! underlying paas provers, checks the resulting account predicate key
//! against the OL's expected `update_vk`, and launches both prover
//! services.

use std::{fs, path::Path, sync::Arc, time::Duration};

use alpen_ee_common::{ChunkStorage, SequencerOLClient};
use alpen_ee_params::AlpenParams;
use eyre::Context;
use k256::schnorr::SigningKey;
use strata_paas::{Prover, ProverBuilder, ProverHandle, ProverServiceBuilder};
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_primitives::buf::Buf32;
use strata_proofimpl_alpen_acct::process_ee_acct_update;
use strata_proofimpl_alpen_chunk::process_ee_chunk;
use strata_proofimpl_predicate_keys::validate_expected_predicate_key;
#[cfg(feature = "sp1")]
use strata_proofimpl_predicate_keys::{PredicateKeyProvider, Sp1Groth16PredicateKey};
use tracing::info;
use zkaleido_native_adapter::NativeHost;
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

use super::{AcctSpec, ChunkSpec, EeBatchProofDbManager, PaasBatchProver};
use crate::{args::ProverBackendConfig, service_executor::ServiceExecutor};

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

/// Picks a prover backend, builds the paas provers, validates the resulting
/// account predicate key against the OL's expected `update_vk`, and
/// launches both prover services.
pub(crate) async fn launch_validated_ee_batch_prover(
    ol_client: &(impl SequencerOLClient + Send + Sync),
    service_executor: &ServiceExecutor,
    builders: EeProverBuilders,
    stores: EeProverStores,
    backend: ProverBackendConfig,
    params: Arc<AlpenParams>,
) -> eyre::Result<Arc<PaasBatchProver>> {
    let ol_account_update_vk = ol_client
        .get_latest_account_update_vk()
        .await
        .context("failed to fetch OL account update_vk for prover validation")?;
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
    backend: ProverBackendConfig,
    params: Arc<AlpenParams>,
) -> eyre::Result<EeProverConfig> {
    match backend {
        ProverBackendConfig::Native {
            chunk_signing_key_path,
            acct_signing_key_path,
        } => {
            info!(target: "alpen-client", "EE chunk + acct provers: native host");

            let chunk_signing_key = native_schnorr_signing_key_from_file(&chunk_signing_key_path)?;
            let acct_signing_key = native_schnorr_signing_key_from_file(&acct_signing_key_path)?;

            let chunk_predicate_key = schnorr_predicate_key(&chunk_signing_key);
            let chunk_host = {
                let chunk_params = (*params).clone();
                NativeHost::new(chunk_signing_key, move |zkvm| {
                    process_ee_chunk(zkvm, &chunk_params)
                })
            };
            let chunk = builders.chunk.native(chunk_host);

            let account_predicate_key = schnorr_predicate_key(&acct_signing_key);
            let acct_host = {
                let acct_params = (*params).clone();
                NativeHost::new(acct_signing_key, move |zkvm| {
                    process_ee_acct_update(zkvm, &acct_params, &chunk_predicate_key)
                })
            };
            let account = builders.account.native(acct_host);

            Ok(EeProverConfig {
                provers: EeProvers { chunk, account },
                account_predicate_key,
            })
        }
        #[cfg(feature = "sp1")]
        ProverBackendConfig::Sp1 {
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
        ProverBackendConfig::Sp1 { .. } => Err(eyre::eyre!(
            "remote SP1 prover is not compiled in; pass --prover-backend native \
             to use the native backend instead, or build with the `sp1` feature"
        )),
    }
}

/// Reads a native-prover Schnorr signing key from a hex-encoded key file.
///
/// Mirrors reth's own `--p2p-secret-key` file convention: a bare hex
/// string, no `0x` prefix, optional surrounding whitespace.
fn native_schnorr_signing_key_from_file(path: &Path) -> eyre::Result<SigningKey> {
    let hex = fs::read_to_string(path)
        .with_context(|| format!("failed to read native signing key file {path:?}"))?;
    parse_native_schnorr_signing_key(hex.trim())
        .with_context(|| format!("invalid native signing key file {path:?}"))
}

/// Parses a hex-encoded native-prover Schnorr signing key.
fn parse_native_schnorr_signing_key(hex: &str) -> eyre::Result<SigningKey> {
    let bytes: Buf32 = hex
        .parse()
        .map_err(|e| eyre::eyre!("failed to parse as 32-byte hex: {e}"))?;
    SigningKey::from_bytes(bytes.as_ref())
        .map_err(|e| eyre::eyre!("invalid Schnorr signing key: {e}"))
}

/// Derives the `Bip340Schnorr` predicate key that verifies proofs signed by `signing_key`.
fn schnorr_predicate_key(signing_key: &SigningKey) -> PredicateKey {
    PredicateKey::new(
        PredicateTypeId::Bip340Schnorr,
        signing_key.verifying_key().to_bytes().to_vec(),
    )
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
    use super::*;

    #[test]
    fn parse_native_schnorr_signing_key_accepts_valid_hex() {
        let hex = "11".repeat(32);
        parse_native_schnorr_signing_key(&hex).unwrap();
    }

    #[test]
    fn parse_native_schnorr_signing_key_rejects_wrong_length() {
        // `SigningKey` doesn't implement `Debug`, so `Result::unwrap_err` isn't usable here.
        let Err(err) = parse_native_schnorr_signing_key("1122") else {
            panic!("expected an error for a too-short key");
        };
        assert!(err.to_string().contains("32-byte"));
    }

    #[test]
    fn parse_native_schnorr_signing_key_rejects_invalid_hex() {
        let hex = "zz".repeat(32);
        assert!(parse_native_schnorr_signing_key(&hex).is_err());
    }

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
