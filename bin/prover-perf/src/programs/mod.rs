use std::str::FromStr;

mod alpen_acct;
mod alpen_chunk;

#[cfg(feature = "sp1")]
use zkaleido::ExecutionSummary;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GuestProgram {
    AlpenAcct,
    AlpenChunk,
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

/// Runs SP1 programs and pairs each program's name with its
/// [`ExecutionSummary`] (cycles, gas, public values).
#[cfg(feature = "sp1")]
pub async fn run_sp1_programs(programs: &[GuestProgram]) -> Vec<(String, ExecutionSummary)> {
    use strata_sp1_guest_builder::{GUEST_ALPEN_ACCT_ELF, GUEST_ALPEN_CHUNK_ELF};
    use zkaleido_sp1_host::{SP1Host, SP1HostConfig};

    let mut reports = Vec::with_capacity(programs.len());
    for program in programs {
        let report = match program {
            GuestProgram::AlpenAcct => {
                let host =
                    SP1Host::init_with_config(&GUEST_ALPEN_ACCT_ELF, SP1HostConfig::default())
                        .await;
                alpen_acct::gen_perf_report(&host)
            }
            GuestProgram::AlpenChunk => {
                let host =
                    SP1Host::init_with_config(&GUEST_ALPEN_CHUNK_ELF, SP1HostConfig::default())
                        .await;
                alpen_chunk::gen_perf_report(&host)
            }
        };
        reports.push(report);
    }
    reports
}
