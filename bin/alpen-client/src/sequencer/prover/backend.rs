//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (`sequencer.prover.backend`, native or sp1), builds every
//! configured `[sequencer.prover.programs.<spec_version>]` entry into a
//! launched [`ProverProgram`], keyed by that entry's spec version, and
//! hard-fails unless one of them derives the account predicate key the OL
//! expects as its `update_vk`, warning when that match isn't the program
//! governing current batches. Routing a given batch to the right resident
//! program (by that batch's own governing spec version) is
//! [`PaasBatchProver`]'s job, not this module's.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};

use alpen_ee_common::{BatchStorage, ChunkStorage, SequencerOLClient};
use alpen_ee_params::{AlpenParams, AlpenSpecId};
use eyre::Context;
use k256::schnorr::SigningKey;
use strata_paas::{Prover, ProverBuilder, ProverServiceBuilder};
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_primitives::buf::Buf32;
use strata_proofimpl_alpen_acct::process_ee_acct_update;
use strata_proofimpl_alpen_chunk::process_ee_chunk;
use tracing::{info, warn};
#[cfg(feature = "sp1")]
use zkaleido::ZkVmExecutor;
use zkaleido_native_adapter::NativeHost;
#[cfg(feature = "sp1")]
use zkaleido_sp1_groth16_verifier::SP1Groth16Verifier;
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

use super::{AcctSpec, ChunkSpec, EeBatchProofDbManager, PaasBatchProver, ProverProgram};
use crate::{config::ProverBackendConfig, service_executor::ServiceExecutor};

/// Default end-to-end deadline applied to the SP1 prover network for the EE
/// chunk + acct provers when `sequencer.prover.deadline_secs` is not set. Chosen
/// to comfortably cover chunk/acct proofs while still failing fast on stuck
/// requests.
#[cfg(feature = "sp1")]
const DEFAULT_SP1_DEADLINE_SECS: u64 = 4 * 60 * 60;

/// Builds a fresh, unconfigured-for-any-particular-candidate
/// `ProverBuilder<ChunkSpec>`/`ProverBuilder<AcctSpec>` on demand, scoped to
/// one resident spec version. A `ProverBuilder` is single-use
/// (`.native(host)`/`.remote(host)` consume it), but every resident
/// candidate needs its own, so the caller hands over factories instead of
/// pre-built builders. Each factory must give its builder a
/// [`VersionedTaskStore`](super::VersionedTaskStore) scoped to the passed
/// `AlpenSpecId` — see that type's doc comment for why sharing one task
/// store across simultaneously-live versions would be unsafe.
pub(crate) struct EeProverBuilders {
    pub(crate) chunk: Box<dyn Fn(AlpenSpecId) -> ProverBuilder<ChunkSpec> + Send + Sync>,
    pub(crate) account: Box<dyn Fn(AlpenSpecId) -> ProverBuilder<AcctSpec> + Send + Sync>,
}

pub(crate) struct EeProverStores {
    pub(crate) chunk_storage: Arc<dyn ChunkStorage>,
    pub(crate) batch_storage: Arc<dyn BatchStorage>,
    pub(crate) batch_proofs: Arc<EeBatchProofDbManager>,
}

/// One resident candidate's built, not-yet-launched chunk + acct provers.
struct EeProvers {
    chunk: Prover<ChunkSpec>,
    account: Prover<AcctSpec>,
}

/// Picks a prover backend, builds every resident
/// `[sequencer.prover.programs.<spec_version>]` entry into launched provers,
/// and hard-fails unless one of them derives the account predicate key the
/// OL expects as its `update_vk`.
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
    let governing_version = governing_spec_version(stores.batch_storage.as_ref(), &params).await?;
    let provers = build_ee_provers(
        &builders,
        backend,
        params,
        &ol_account_update_vk,
        governing_version,
    )
    .await?;

    let programs = launch_ee_prover_services(service_executor, provers).await?;

    Ok(Arc::new(PaasBatchProver::new(
        programs,
        stores.chunk_storage,
        stores.batch_storage,
        stores.batch_proofs,
    )))
}

/// Builds every configured program, keyed by the `AlpenSpecId` it is
/// configured under, and hard-fails unless one of them matches
/// `ol_account_update_vk`, warning when that match isn't the program keyed
/// under `governing_version`.
///
/// `backend` may carry several programs (see
/// [`ProverProgramPaths`](crate::config::ProverProgramPaths)'s doc comment):
/// an operator straddling a VK rotation configures both the currently-active
/// and the not-yet-active program ahead of time, so the sequencer doesn't
/// need restarting once the rotation lands — as long as the successor
/// version's program was already resident. Every program is built (not just
/// whichever is active right now), since [`PaasBatchProver`] routes each
/// batch's proof request by that batch's own governing spec version, not by
/// whatever's active at this moment.
async fn build_ee_provers(
    builders: &EeProverBuilders,
    backend: ProverBackendConfig,
    params: Arc<AlpenParams>,
    ol_account_update_vk: &PredicateKey,
    governing_version: AlpenSpecId,
) -> eyre::Result<BTreeMap<AlpenSpecId, EeProvers>> {
    let mut provers = BTreeMap::new();
    let mut matched_versions: Vec<AlpenSpecId> = Vec::new();

    match backend {
        ProverBackendConfig::Native { programs } => {
            info!(target: "alpen-client", "EE chunk + acct provers: native host");

            for (spec_version, program) in &programs {
                let chunk_signing_key = native_schnorr_signing_key_from_file(&program.chunk_path)?;
                let acct_signing_key = native_schnorr_signing_key_from_file(&program.acct_path)?;

                let account_predicate_key = schnorr_predicate_key(&acct_signing_key);
                if &account_predicate_key == ol_account_update_vk {
                    matched_versions.push(*spec_version);
                }

                let chunk_predicate_key = schnorr_predicate_key(&chunk_signing_key);
                let chunk_host = {
                    let chunk_params = (*params).clone();
                    let spec_version = *spec_version;
                    NativeHost::new(chunk_signing_key, move |zkvm| {
                        process_ee_chunk(zkvm, &chunk_params, spec_version)
                    })
                };
                let chunk = (builders.chunk)(*spec_version).native(chunk_host);

                let acct_host = {
                    let acct_params = (*params).clone();
                    let spec_version = *spec_version;
                    NativeHost::new(acct_signing_key, move |zkvm| {
                        process_ee_acct_update(
                            zkvm,
                            &acct_params,
                            spec_version,
                            &chunk_predicate_key,
                        )
                    })
                };
                let account = (builders.account)(*spec_version).native(acct_host);

                provers.insert(*spec_version, EeProvers { chunk, account });
            }
        }
        #[cfg(feature = "sp1")]
        ProverBackendConfig::Sp1 {
            programs,
            deadline_secs,
        } => {
            let deadline_secs = deadline_secs.unwrap_or(DEFAULT_SP1_DEADLINE_SECS);
            let deadline = Duration::from_secs(deadline_secs);
            let sp1_config = SP1HostConfig::default().with_deadline(deadline);

            for (spec_version, program) in &programs {
                info!(
                    target: "alpen-client",
                    deadline_secs,
                    %spec_version,
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
                // configured together in one table, not that they're
                // actually a matched build output. There's no way to
                // introspect an ELF's compiled-in chunk-VK dependency from
                // outside it, so this has to be trusted from build
                // provenance rather than checked here.
                let chunk_host = SP1Host::init_with_config(&chunk_elf, sp1_config.clone()).await;
                let acct_host = SP1Host::init_with_config(&acct_elf, sp1_config.clone()).await;
                let account_predicate_key = sp1_groth16_predicate_key(acct_host.program_id().0)
                    .context("failed to derive local SP1 account prover predicate key")?;

                if &account_predicate_key == ol_account_update_vk {
                    matched_versions.push(*spec_version);
                }

                let chunk = (builders.chunk)(*spec_version).remote(chunk_host);
                let account = (builders.account)(*spec_version).remote(acct_host);

                provers.insert(*spec_version, EeProvers { chunk, account });
            }
        }
        #[cfg(not(feature = "sp1"))]
        ProverBackendConfig::Sp1 { .. } => {
            return Err(eyre::eyre!(
                "remote SP1 prover is not compiled in; set sequencer.prover.backend = \"native\" \
                 to use the native backend instead, or build with the `sp1` feature"
            ));
        }
    }

    validate_live_vk_match(
        &matched_versions,
        governing_version,
        provers.len(),
        ol_account_update_vk,
    )?;

    Ok(provers)
}

/// Resolves the spec version governing the batches being proved right now:
/// the one stamped on the most recently sealed batch, which is exactly what
/// [`PaasBatchProver::program_for`] routes on.
///
/// A node with no batch sealed yet is at genesis, where the schedule names
/// the version outright.
async fn governing_spec_version(
    batch_storage: &dyn BatchStorage,
    params: &AlpenParams,
) -> eyre::Result<AlpenSpecId> {
    let latest = batch_storage
        .get_latest_batch()
        .await
        .context("failed to read the latest batch for prover validation")?;

    Ok(match latest {
        Some((batch, _status)) => batch.spec_version(),
        None => params.spec_schedule().active_at(0),
    })
}

/// Checks the resident programs against the OL's live `update_vk`.
///
/// Fatal only when *no* resident program derives the live key: that node can
/// never produce an account proof OL accepts, whatever version it ends up
/// proving.
///
/// A match under a version other than `governing_version` only warns, and
/// deliberately so. The two inputs are read at different freshness and move
/// independently, so they disagree during any rotation, in both directions:
///
/// - `get_latest_account_update_vk` reads OL's `Latest` tag, so the live key flips as soon as the
///   update carrying a rotation lands at the OL tip, while batches keep being sealed under the old
///   version until the rotation-consuming block's batch is sealed.
/// - The sequencer builds ahead of OL, so it can seal batches under the new version before the
///   update that rotates OL's key has been accepted.
///
/// Failing startup on either window would wedge the node: nothing else runs,
/// so the very state the check is waiting on can never advance, and the node
/// restart-loops. A warning naming both versions is what this can soundly
/// assert — it still surfaces transposed
/// `[sequencer.prover.programs.<version>]` keys, which is the case worth
/// catching, without being able to stall a healthy node.
fn validate_live_vk_match(
    matched_versions: &[AlpenSpecId],
    governing_version: AlpenSpecId,
    program_count: usize,
    ol_account_update_vk: &PredicateKey,
) -> eyre::Result<()> {
    if matched_versions.is_empty() {
        return Err(eyre::eyre!(
            "none of the {program_count} resident prover program(s) match \
             OL's expected account update_vk {ol_account_update_vk:?}"
        ));
    }

    if !matched_versions.contains(&governing_version) {
        let matched = matched_versions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        warn!(
            target: "alpen-client",
            %governing_version,
            matched_versions = %matched,
            "the prover program matching OL's live account update_vk is not the one keyed \
             under the version governing current batches; expected during a rotation, but \
             check the [sequencer.prover.programs.<version>] keys are not transposed"
        );
    }

    Ok(())
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
    PredicateKey::try_new(
        PredicateTypeId::Bip340Schnorr,
        signing_key.verifying_key().to_bytes().to_vec(),
    )
    .expect("verifying key fits within the condition length limit")
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

    PredicateKey::try_new(
        PredicateTypeId::Sp1Groth16,
        sp1_verifier.to_uncompressed_bytes(),
    )
    .map_err(|e| eyre::eyre!("SP1 Groth16 verifier does not fit in a predicate key: {e}"))
}

/// Launches every resident candidate's built provers into paas services,
/// keyed by the same `AlpenSpecId` [`build_ee_provers`] indexed them by.
async fn launch_ee_prover_services(
    service_executor: &ServiceExecutor,
    provers: BTreeMap<AlpenSpecId, EeProvers>,
) -> eyre::Result<BTreeMap<AlpenSpecId, ProverProgram>> {
    let prover_tick = Duration::from_secs(5);
    let mut programs = BTreeMap::new();
    for (spec_version, provers) in provers {
        let chunk_handle = ProverServiceBuilder::new(provers.chunk)
            .tick_interval(prover_tick)
            .launch(service_executor)
            .await
            .map_err(|e| eyre::eyre!("launching chunk prover service for {spec_version}: {e}"))?;
        let acct_handle = ProverServiceBuilder::new(provers.account)
            .tick_interval(prover_tick)
            .launch(service_executor)
            .await
            .map_err(|e| eyre::eyre!("launching acct prover service for {spec_version}: {e}"))?;

        programs.insert(
            spec_version,
            ProverProgram {
                chunk_handle,
                acct_handle,
            },
        );
    }

    Ok(programs)
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

    fn live_vk() -> PredicateKey {
        schnorr_predicate_key(&parse_native_schnorr_signing_key(&"11".repeat(32)).unwrap())
    }

    #[test]
    fn live_vk_match_at_the_governing_version_is_accepted() {
        validate_live_vk_match(&[AlpenSpecId::V0], AlpenSpecId::V0, 2, &live_vk()).unwrap();
    }

    #[test]
    fn live_vk_match_off_the_governing_version_only_warns() {
        // Both rotation windows land here, in either direction: OL's live key
        // flips at its own tip while batches still seal under the old
        // version, and the sequencer seals under the new version before the
        // rotating update is accepted. Neither may fail startup.
        validate_live_vk_match(&[AlpenSpecId::V1], AlpenSpecId::V0, 2, &live_vk()).unwrap();
        validate_live_vk_match(&[AlpenSpecId::V0], AlpenSpecId::V1, 2, &live_vk()).unwrap();
    }

    #[test]
    fn no_live_vk_match_is_rejected() {
        let err = validate_live_vk_match(&[], AlpenSpecId::V0, 2, &live_vk())
            .expect_err("no matching program must fail startup");
        assert!(err.to_string().contains("none of the 2 resident"));
    }
}
