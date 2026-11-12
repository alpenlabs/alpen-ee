//! EE chunk + acct prover backend selection and launch.
//!
//! [`launch_validated_ee_batch_prover`] is the entry point: it picks a
//! backend (`sequencer.prover.backend`, native or sp1), builds every
//! configured `[[sequencer.prover.programs]]` entry into a launched
//! [`ProverProgram`] with the account predicate key it derives, and hard-fails
//! unless one of them derives the key the OL expects as its `update_vk`.
//! Routing a given batch to the right resident program (by the key the OL
//! checks that batch against) is [`PaasBatchProver`]'s job, not this
//! module's.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};

use alpen_common::SequencerOLClient;
use alpen_params::{AlpenParams, AlpenSpecId};
use alpen_proof_acct::process_ee_acct_update_native;
use alpen_proof_chunk::process_ee_chunk_native;
use alpen_storage_api::{BatchStorage, ChunkStorage, ExecBlockStorage};
use alpen_storage_store::{BatchTaskKey, ChunkTaskKey, ProgramScope, ProverDbMdbx, ProverTaskKey};
use eyre::Context;
use k256::schnorr::SigningKey;
use strata_paas::{Prover, ProverBuilder, ProverServiceBuilder};
use strata_predicate::{PredicateKey, PredicateTypeId};
use strata_primitives::buf::Buf32;
use tracing::info;
#[cfg(feature = "sp1")]
use zkaleido::ZkVmExecutor;
use zkaleido_native_adapter::NativeHost;
#[cfg(feature = "sp1")]
use zkaleido_sp1_groth16_verifier::SP1Groth16Verifier;
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

use super::{
    AcctSpec, BatchProverSources, ChunkSpec, EeBatchProofDbManager, PaasBatchProver, ProverProgram,
};
use crate::{config::ProverBackendConfig, service_executor::ServiceExecutor};

/// Default end-to-end deadline applied to the SP1 prover network for the EE
/// chunk + acct provers when `sequencer.prover.deadline_secs` is not set. Chosen
/// to comfortably cover chunk/acct proofs while still failing fast on stuck
/// requests.
#[cfg(feature = "sp1")]
const DEFAULT_SP1_DEADLINE_SECS: u64 = 4 * 60 * 60;

/// How often each launched prover service polls for work.
const PROVER_TICK: Duration = Duration::from_secs(5);

/// Builds the chunk prover of the program with the given scope.
pub(crate) type ChunkProverFactory =
    Box<dyn Fn(&ProgramScope) -> ProverBuilder<ChunkSpec> + Send + Sync>;

/// Builds the acct prover of the program with the given scope.
pub(crate) type AcctProverFactory =
    Box<dyn Fn(&ProgramScope) -> ProverBuilder<AcctSpec> + Send + Sync>;

/// Builds a fresh, unconfigured-for-any-particular-candidate
/// `ProverBuilder<ChunkSpec>`/`ProverBuilder<AcctSpec>` on demand, scoped to
/// one resident program. A `ProverBuilder` is single-use
/// (`.native(host)`/`.remote(host)` consume it), but every resident
/// candidate needs its own, so the caller hands over factories instead of
/// pre-built builders. Each factory must give its builder an
/// [`EeTaskStore`](super::storage::EeTaskStore) scoped to the passed
/// [`ProgramScope`] — see that type's doc comment for why sharing one task
/// store across simultaneously-live programs would be unsafe.
pub(crate) struct EeProverBuilders {
    pub(crate) chunk: ChunkProverFactory,
    pub(crate) account: AcctProverFactory,
}

pub(crate) struct EeProverStores {
    pub(crate) chunk_storage: Arc<dyn ChunkStorage>,
    pub(crate) batch_storage: Arc<dyn BatchStorage>,
    pub(crate) exec_storage: Arc<dyn ExecBlockStorage>,
    pub(crate) batch_proofs: Arc<EeBatchProofDbManager>,
    pub(crate) prover_db: Arc<ProverDbMdbx>,
}

/// A resident program whose chunk + acct provers are built but not yet
/// launched.
type BuiltProgram = ProverProgram<Prover<ChunkSpec>, Prover<AcctSpec>>;

impl BuiltProgram {
    /// Launches the chunk and acct provers into paas services.
    async fn launch(self, service_executor: &ServiceExecutor) -> eyre::Result<ProverProgram> {
        let spec_version = self.scope.spec_version();
        let chunk = ProverServiceBuilder::new(self.chunk)
            .tick_interval(PROVER_TICK)
            .launch(service_executor)
            .await
            .map_err(|e| eyre::eyre!("launching chunk prover service for {spec_version}: {e}"))?;
        let acct = ProverServiceBuilder::new(self.acct)
            .tick_interval(PROVER_TICK)
            .launch(service_executor)
            .await
            .map_err(|e| eyre::eyre!("launching acct prover service for {spec_version}: {e}"))?;

        Ok(ProverProgram {
            scope: self.scope,
            chunk,
            acct,
        })
    }
}

/// Picks a prover backend, builds every resident
/// `[[sequencer.prover.programs]]` entry into launched provers,
/// and hard-fails unless one of them derives the account predicate key the
/// OL expects as its `update_vk`.
pub(crate) async fn launch_validated_ee_batch_prover(
    ol_client: Arc<dyn SequencerOLClient + Send + Sync>,
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

    let programs_per_version = count_programs_per_version(&backend);
    ensure_no_legacy_open_tasks(&stores.prover_db, &programs_per_version)?;

    let built = build_programs(&builders, backend, params, &ol_account_update_vk).await?;
    adopt_legacy_tasks(&stores.prover_db, &built, &programs_per_version)?;

    let mut programs = Vec::with_capacity(built.len());
    for program in built {
        programs.push(program.launch(service_executor).await?);
    }

    Ok(Arc::new(PaasBatchProver::new(
        programs,
        BatchProverSources {
            ol_client,
            chunk_storage: stores.chunk_storage,
            batch_storage: stores.batch_storage,
            exec_storage: stores.exec_storage,
            batch_proofs: stores.batch_proofs,
        },
    )))
}

/// How many configured programs prove each spec version.
fn count_programs_per_version(backend: &ProverBackendConfig) -> BTreeMap<AlpenSpecId, usize> {
    let mut counts = BTreeMap::new();
    for program in backend.programs() {
        *counts.entry(program.spec_version).or_insert(0) += 1;
    }
    counts
}

/// Fails when a spec version with more than one program has an unfinished
/// legacy task.
///
/// A legacy task was stored before tasks recorded their program. A version's
/// only program adopts those, but with two programs neither can tell whether
/// the task is its own, so no prover would ever pick it up and its batch would
/// stall. Running with one program for that version until they finish lets
/// that program adopt them.
fn ensure_no_legacy_open_tasks(
    prover_db: &ProverDbMdbx,
    programs_per_version: &BTreeMap<AlpenSpecId, usize>,
) -> eyre::Result<()> {
    for (spec_version, count) in programs_per_version {
        if *count < 2 {
            continue;
        }
        let open = count_legacy_open_tasks::<ChunkTaskKey>(prover_db, *spec_version)?
            + count_legacy_open_tasks::<BatchTaskKey>(prover_db, *spec_version)?;
        if open > 0 {
            return Err(eyre::eyre!(
                "{open} unfinished prover task(s) of spec version {spec_version} were stored \
                 before tasks recorded their program, and {count} programs are configured for \
                 {spec_version}; start with one {spec_version} program until they finish"
            ));
        }
    }
    Ok(())
}

fn count_legacy_open_tasks<K: ProverTaskKey>(
    prover_db: &ProverDbMdbx,
    spec_version: AlpenSpecId,
) -> eyre::Result<usize> {
    prover_db
        .count_legacy_open_tasks::<K>(spec_version)
        .context("failed to read the prover task tables")
}

/// Gives each version's legacy tasks, chunk and acct, to that version's
/// program when it has only one.
///
/// A legacy task was stored before tasks recorded their program, and no
/// program works on one until it is adopted. A version with two programs
/// keeps its legacy tasks, which [`ensure_no_legacy_open_tasks`] has already
/// checked are all finished. paas re-spawns unfinished tasks when its prover
/// launches, so this has to run before that.
fn adopt_legacy_tasks(
    prover_db: &ProverDbMdbx,
    programs: &[BuiltProgram],
    programs_per_version: &BTreeMap<AlpenSpecId, usize>,
) -> eyre::Result<()> {
    for program in programs {
        let scope = &program.scope;
        let spec_version = scope.spec_version();
        if programs_per_version.get(&spec_version) != Some(&1) {
            continue;
        }
        let adopted = adopt_legacy_tasks_of::<ChunkTaskKey>(prover_db, scope)?
            + adopt_legacy_tasks_of::<BatchTaskKey>(prover_db, scope)?;
        if adopted > 0 {
            info!(
                target: "alpen-client",
                %spec_version,
                adopted,
                "gave prover tasks stored before tasks recorded their program to the version's program"
            );
        }
    }
    Ok(())
}

fn adopt_legacy_tasks_of<K: ProverTaskKey>(
    prover_db: &ProverDbMdbx,
    scope: &ProgramScope,
) -> eyre::Result<usize> {
    prover_db
        .adopt_legacy_tasks::<K>(scope)
        .context("failed to adopt legacy prover tasks")
}

/// Builds every configured program, and hard-fails unless one of them
/// matches `ol_account_update_vk` and no two share a key.
///
/// `backend` may carry several programs (see
/// [`ProverProgramConfig`](crate::config::ProverProgramConfig)'s doc comment):
/// an operator straddling a VK rotation configures both the currently-active
/// and the not-yet-active program ahead of time, so the sequencer doesn't
/// need restarting once the rotation lands — as long as the target program
/// was already resident. Every program is built (not just whichever is active
/// right now), since [`PaasBatchProver`] routes each batch's proof request by
/// the key the OL checks that batch against, not by whatever's active at this
/// moment.
async fn build_programs(
    builders: &EeProverBuilders,
    backend: ProverBackendConfig,
    params: Arc<AlpenParams>,
    ol_account_update_vk: &PredicateKey,
) -> eyre::Result<Vec<BuiltProgram>> {
    let mut built = Vec::new();

    match backend {
        ProverBackendConfig::Native { programs } => {
            info!(target: "alpen-client", "EE chunk + acct provers: native host");

            for program in &programs {
                let spec_version = &program.spec_version;
                let chunk_signing_key = native_schnorr_signing_key_from_file(&program.chunk_path)?;
                let acct_signing_key = native_schnorr_signing_key_from_file(&program.acct_path)?;

                let account_predicate_key = schnorr_predicate_key(&acct_signing_key);
                let scope = ProgramScope::new(*spec_version, account_predicate_key);

                let chunk_predicate_key = schnorr_predicate_key(&chunk_signing_key);
                let chunk_host = {
                    let chunk_params = (*params).clone();
                    let spec_version = *spec_version;
                    NativeHost::new(chunk_signing_key, move |zkvm| {
                        process_ee_chunk_native(zkvm, &chunk_params, spec_version)
                    })
                };
                let chunk = (builders.chunk)(&scope).native(chunk_host);

                let acct_host = {
                    let spec_version = *spec_version;
                    NativeHost::new(acct_signing_key, move |zkvm| {
                        process_ee_acct_update_native(zkvm, spec_version, &chunk_predicate_key)
                    })
                };
                let acct = (builders.account)(&scope).native(acct_host);

                built.push(BuiltProgram { scope, chunk, acct });
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

            for program in &programs {
                let spec_version = &program.spec_version;
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
                // `ProverProgramConfig` only guarantees these two paths were
                // configured together in one table, not that they're
                // actually a matched build output. There's no way to
                // introspect an ELF's compiled-in chunk-VK dependency from
                // outside it, so this has to be trusted from build
                // provenance rather than checked here.
                let chunk_host = SP1Host::init_with_config(&chunk_elf, sp1_config.clone()).await;
                let acct_host = SP1Host::init_with_config(&acct_elf, sp1_config.clone()).await;
                let account_predicate_key = sp1_groth16_predicate_key(acct_host.program_id().0)
                    .context("failed to derive local SP1 account prover predicate key")?;

                let scope = ProgramScope::new(*spec_version, account_predicate_key);

                let chunk = (builders.chunk)(&scope).remote(chunk_host);
                let acct = (builders.account)(&scope).remote(acct_host);

                built.push(BuiltProgram { scope, chunk, acct });
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

    let program_keys: Vec<&PredicateKey> = built.iter().map(|p| p.scope.key()).collect();
    validate_live_vk_match(&program_keys, ol_account_update_vk)?;
    ensure_distinct_keys(&program_keys)?;

    Ok(built)
}

/// Fails when two resident programs derive the same key.
///
/// The OL tells programs apart only by key, so a batch's key would match both,
/// and the block builder could not tell which version a rotation to that key
/// activates. Checked before the prover services start, so they never run
/// with an ambiguous key.
fn ensure_distinct_keys(program_keys: &[&PredicateKey]) -> eyre::Result<()> {
    for (idx, key) in program_keys.iter().enumerate() {
        if program_keys[..idx].contains(key) {
            return Err(eyre::eyre!("two prover programs derive the same key {key}"));
        }
    }
    Ok(())
}

/// Checks the resident programs against the OL's live `update_vk`.
///
/// Fatal when no resident program derives the live key. The next update the
/// OL accepts is checked against that key, so that node could never produce
/// an account proof the OL accepts. Which batch each program proves is decided
/// per batch, by [`PaasBatchProver`].
fn validate_live_vk_match(
    program_keys: &[&PredicateKey],
    ol_account_update_vk: &PredicateKey,
) -> eyre::Result<()> {
    if !program_keys.contains(&ol_account_update_vk) {
        return Err(eyre::eyre!(
            "none of the {} resident prover program(s) match \
             OL's expected account update_vk {ol_account_update_vk:?}",
            program_keys.len()
        ));
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

    fn signing_key_predicate(byte: &str) -> PredicateKey {
        schnorr_predicate_key(&parse_native_schnorr_signing_key(&byte.repeat(32)).unwrap())
    }

    #[test]
    fn a_program_matching_the_live_vk_is_accepted() {
        let live = signing_key_predicate("11");
        let other = signing_key_predicate("22");
        validate_live_vk_match(&[&other, &live], &live).unwrap();
    }

    #[test]
    fn no_live_vk_match_is_rejected() {
        let live = signing_key_predicate("11");
        let other = signing_key_predicate("22");
        let err = validate_live_vk_match(&[&other, &other], &live)
            .expect_err("no matching program must fail startup");
        assert!(err.to_string().contains("none of the 2 resident"));
    }

    #[test]
    fn programs_with_distinct_keys_are_accepted() {
        let first = signing_key_predicate("11");
        let second = signing_key_predicate("22");
        ensure_distinct_keys(&[&first, &second]).unwrap();
    }

    #[test]
    fn two_programs_with_one_key_are_rejected() {
        let first = signing_key_predicate("11");
        let second = signing_key_predicate("22");
        let err = ensure_distinct_keys(&[&first, &second, &first])
            .expect_err("a shared key must fail startup");
        assert!(err.to_string().contains("derive the same key"));
    }
}
