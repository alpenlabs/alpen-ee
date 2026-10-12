use std::{fmt, str::FromStr};
#[cfg(feature = "sp1")]
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(feature = "sp1")]
use alpen_sp1_guest_builder::{GUEST_ALPEN_ACCT_ELF_PATH, GUEST_ALPEN_CHUNK_ELF_PATH};
#[cfg(feature = "sp1")]
use sp1_sdk::SP1_CIRCUIT_VERSION;
#[cfg(feature = "sp1")]
use tracing::info;
#[cfg(feature = "sp1")]
use zkaleido::{ExecutionSummary, ProofReceiptWithMetadata, ZkVm};
#[cfg(feature = "sp1")]
use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

mod alpen_acct;
mod alpen_chunk;
mod da;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GuestProgram {
    // Declared in proving order: the account guest verifies the chunk proof.
    AlpenChunk,
    AlpenAcct,
}

impl GuestProgram {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlpenChunk => "alpen-chunk",
            Self::AlpenAcct => "alpen-acct",
        }
    }
}

impl fmt::Display for GuestProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GuestProgram {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "alpen-acct" => Ok(GuestProgram::AlpenAcct),
            "alpen-chunk" => Ok(GuestProgram::AlpenChunk),
            _ => Err(format!("unknown program: {s}")),
        }
    }
}

/// Directory the generated proofs are kept in.
#[cfg(feature = "sp1")]
const PROOFS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/proofs");

/// Path of `program`'s generated SP1 proof, named the way zkaleido's
/// `ProofReceiptWithMetadata::save` names it.
#[cfg(feature = "sp1")]
fn proof_path(program: GuestProgram) -> PathBuf {
    Path::new(PROOFS_DIR).join(format!(
        "{program}_{}_{SP1_CIRCUIT_VERSION}.proof",
        ZkVm::SP1
    ))
}

#[cfg(feature = "sp1")]
fn load_proof(program: GuestProgram) -> ProofReceiptWithMetadata {
    let path = proof_path(program);
    ProofReceiptWithMetadata::load(&path).unwrap_or_else(|e| {
        panic!(
            "load {program} proof from {}: {e}; generate it with `just prover-proof`",
            path.display()
        )
    })
}

#[cfg(feature = "sp1")]
fn save_proof(program: GuestProgram, proof: &ProofReceiptWithMetadata) {
    let path = proof_path(program);
    fs::create_dir_all(PROOFS_DIR).unwrap_or_else(|e| panic!("create {PROOFS_DIR}: {e}"));
    fs::write(&path, proof.encode())
        .unwrap_or_else(|e| panic!("write {program} proof to {}: {e}", path.display()));
}

/// Runs SP1 programs and pairs each program's name with its
/// [`ExecutionSummary`] (cycles, gas, public values).
///
/// The report diffs against the last merged PR by program name, so renaming a
/// program drops its deltas for one run.
#[cfg(feature = "sp1")]
pub async fn run_sp1_programs(programs: &[GuestProgram]) -> Vec<(String, ExecutionSummary)> {
    let mut reports = Vec::with_capacity(programs.len());
    for program in programs {
        let host = sp1_host(*program).await;
        let report = match program {
            GuestProgram::AlpenAcct => {
                alpen_acct::gen_perf_report(&host, &load_proof(GuestProgram::AlpenChunk))
            }
            GuestProgram::AlpenChunk => alpen_chunk::gen_perf_report(&host),
        };
        reports.push(report);
    }
    reports
}

/// Proves `programs` with SP1 and saves each proof under `proofs/`.
///
/// Runs them in proving order whatever order they were given in, and saves
/// each proof before the next program starts. The account guest reads the
/// chunk proof from disk, so proving both always pairs the account proof with
/// the fresh chunk proof.
#[cfg(feature = "sp1")]
pub async fn gen_and_save_sp1_proofs(programs: &[GuestProgram]) {
    let mut programs = programs.to_vec();
    programs.sort();
    programs.dedup();

    for program in programs {
        let host = sp1_host(program).await;
        let proof = match program {
            GuestProgram::AlpenChunk => alpen_chunk::gen_proof(&host),
            GuestProgram::AlpenAcct => {
                alpen_acct::gen_proof(&host, &load_proof(GuestProgram::AlpenChunk))
            }
        };
        save_proof(program, &proof);
        info!(%program, path = %proof_path(program).display(), "saved proof");
    }
}

#[cfg(feature = "sp1")]
async fn sp1_host(program: GuestProgram) -> SP1Host {
    let elf_path = match program {
        GuestProgram::AlpenAcct => GUEST_ALPEN_ACCT_ELF_PATH,
        GuestProgram::AlpenChunk => GUEST_ALPEN_CHUNK_ELF_PATH,
    };
    let elf = fs::read(elf_path)
        .unwrap_or_else(|e| panic!("failed to read guest elf at {elf_path}: {e}"));
    SP1Host::init_with_config(&elf, SP1HostConfig::default()).await
}
