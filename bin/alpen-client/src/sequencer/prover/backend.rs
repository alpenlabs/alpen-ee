//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (`--prover-backend native`, the default, or `sp1`), builds the
//! underlying paas provers from whichever configured `--prover-program`
//! candidate's derived account predicate key matches the OL's expected
//! `update_vk`, and launches both prover services.

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
use tracing::info;
#[cfg(feature = "sp1")]
use zkaleido::ZkVmExecutor;
use zkaleido_native_adapter::NativeHost;
#[cfg(feature = "sp1")]
use zkaleido_sp1_groth16_verifier::SP1Groth16Verifier;
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
    // TODO: this resolves the matching candidate once, against whatever
    // update_vk is active at process startup, and keeps using it for the
    // process's lifetime. If OL rotates update_vk again while the sequencer
    // keeps running (no restart), the resolved candidate goes stale and the
    // next proof this process generates fails OL's verification the moment
    // it's submitted, since it's checked against the account state current
    // at that later point, not this startup snapshot. Re-resolving against
    // the actually-current account state at generation/submission time
    // (rather than once here) is a separate follow-up.
    let ol_account_update_vk = ol_client
        .get_latest_account_update_vk()
        .await
        .context("failed to fetch OL account update_vk for prover validation")?;
    let provers = build_ee_provers(builders, backend, params, &ol_account_update_vk).await?;

    let (chunk_handle, acct_handle) = launch_ee_prover_services(service_executor, provers).await?;

    Ok(Arc::new(PaasBatchProver::new(
        chunk_handle,
        acct_handle,
        stores.chunk_storage,
        stores.batch_proofs,
    )))
}

/// Builds the paas provers from whichever `--prover-program` candidate's
/// derived account predicate key matches `ol_account_update_vk`.
///
/// `backend` may carry several candidates (see [`ProverProgramPaths`]'s
/// doc comment): an operator straddling a VK rotation can hand the
/// sequencer both the currently-active and the not-yet-active program so it
/// doesn't need restarting once the rotation lands. Only the matching
/// candidate is actually built into launchable provers; the rest are
/// discarded after their predicate key is checked.
async fn build_ee_provers(
    builders: EeProverBuilders,
    backend: ProverBackendConfig,
    params: Arc<AlpenParams>,
    ol_account_update_vk: &PredicateKey,
) -> eyre::Result<EeProvers> {
    match backend {
        ProverBackendConfig::Native { programs } => {
            info!(target: "alpen-client", "EE chunk + acct provers: native host");

            let candidate_count = programs.len();
            for program in &programs {
                let chunk_signing_key = native_schnorr_signing_key_from_file(&program.chunk_path)?;
                let acct_signing_key = native_schnorr_signing_key_from_file(&program.acct_path)?;

                let account_predicate_key = schnorr_predicate_key(&acct_signing_key);
                if &account_predicate_key != ol_account_update_vk {
                    continue;
                }

                let chunk_predicate_key = schnorr_predicate_key(&chunk_signing_key);
                let chunk_host = {
                    let chunk_params = (*params).clone();
                    NativeHost::new(chunk_signing_key, move |zkvm| {
                        process_ee_chunk(zkvm, &chunk_params)
                    })
                };
                let chunk = builders.chunk.native(chunk_host);

                let acct_host = {
                    let acct_params = (*params).clone();
                    NativeHost::new(acct_signing_key, move |zkvm| {
                        process_ee_acct_update(zkvm, &acct_params, &chunk_predicate_key)
                    })
                };
                let account = builders.account.native(acct_host);

                return Ok(EeProvers { chunk, account });
            }

            Err(no_matching_candidate_err(
                candidate_count,
                ol_account_update_vk,
            ))
        }
        #[cfg(feature = "sp1")]
        ProverBackendConfig::Sp1 {
            programs,
            deadline_secs,
        } => {
            let deadline_secs = deadline_secs.unwrap_or(DEFAULT_SP1_DEADLINE_SECS);
            let deadline = Duration::from_secs(deadline_secs);
            let sp1_config = SP1HostConfig::default().with_deadline(deadline);

            let candidate_count = programs.len();
            for program in &programs {
                info!(
                    target: "alpen-client",
                    deadline_secs,
                    chunk_path = ?program.chunk_path,
                    acct_path = ?program.acct_path,
                    "sp1 EE prover deadline configured"
                );

                let chunk_elf = fs::read(&program.chunk_path).with_context(|| {
                    format!(
                        "failed to read chunk guest ELF at {}",
                        program.chunk_path.display()
                    )
                })?;
                let acct_elf = fs::read(&program.acct_path).with_context(|| {
                    format!(
                        "failed to read account guest ELF at {}",
                        program.acct_path.display()
                    )
                })?;
                // `ProverProgramPaths` only guarantees these two paths were
                // passed together as one `--prover-program` token, not that
                // they're actually a matched build output. There's no way to
                // introspect an ELF's compiled-in chunk-VK dependency from
                // outside it, so this has to be trusted from build
                // provenance rather than checked here.
                let chunk_host = SP1Host::init_with_config(&chunk_elf, sp1_config.clone()).await;
                let acct_host = SP1Host::init_with_config(&acct_elf, sp1_config.clone()).await;
                let account_predicate_key = sp1_groth16_predicate_key(acct_host.program_id().0)
                    .context("failed to derive local SP1 account prover predicate key")?;

                if &account_predicate_key != ol_account_update_vk {
                    continue;
                }

                return Ok(EeProvers {
                    chunk: builders.chunk.remote(chunk_host),
                    account: builders.account.remote(acct_host),
                });
            }

            Err(no_matching_candidate_err(
                candidate_count,
                ol_account_update_vk,
            ))
        }
        #[cfg(not(feature = "sp1"))]
        ProverBackendConfig::Sp1 { .. } => Err(eyre::eyre!(
            "remote SP1 prover is not compiled in; pass --prover-backend native \
             to use the native backend instead, or build with the `sp1` feature"
        )),
    }
}

/// Error for when none of the configured `--prover-program` candidates'
/// derived account predicate keys match the OL's expected `update_vk`.
fn no_matching_candidate_err(
    candidate_count: usize,
    ol_account_update_vk: &PredicateKey,
) -> eyre::Error {
    eyre::eyre!(
        "none of the {candidate_count} configured --prover-program candidate(s) match \
         OL's expected account update_vk {ol_account_update_vk:?}"
    )
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

/// Derives the `Sp1Groth16` predicate key that verifies proofs from the SP1 program
/// identified by `program_id`.
#[cfg(feature = "sp1")]
fn sp1_groth16_predicate_key(program_id: [u8; 32]) -> eyre::Result<PredicateKey> {
    let sp1_verifier = SP1Groth16Verifier::load(
        &sp1_verifier::GROTH16_VK_BYTES,
        program_id,
        *sp1_verifier::VK_ROOT_BYTES,
        true,
    )
    .map_err(|e| eyre::eyre!("failed to load SP1 Groth16 verifier: {e}"))?;

    Ok(PredicateKey::new(
        PredicateTypeId::Sp1Groth16,
        sp1_verifier.to_uncompressed_bytes(),
    ))
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
}
