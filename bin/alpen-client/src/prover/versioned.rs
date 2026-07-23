//! Version-aware native prove strategy for the acct prover.
//!
//! A live VK rotation means batches before the boundary must be proven under
//! the old key and batches after it under the new one — a single fixed host
//! cannot express that. This strategy holds one native host per resident VK
//! and routes each proof to the host matching the VK its batch was stamped
//! with at seal time, so the prover never has to re-derive which key a batch
//! needs.
//!
//! Fails closed: a batch stamped with a VK no host is registered for is a
//! mis-provisioned rollout (the "new ELF" is not resident), and proving under
//! any other key would only burn compute on a proof OL rejects.
// TODO(STR-4002): give the SP1 remote path the same treatment — a registry of
// SP1 hosts keyed by their derived Groth16 VKs, populated from an ELF
// manifest, with fail-closed startup validation of the resident set.

use strata_paas::{ProveContext, ProveStrategy, ProverError, ProverResult, ZkVmProgram};
use strata_predicate::PredicateKey;
use zkaleido::ProofReceiptWithMetadata;
use zkaleido_native_adapter::NativeHost;

use crate::prover::AcctSpec;

/// Routes each acct proof to the native host matching its batch's stamped VK.
pub(crate) struct VersionedNativeAcctStrategy {
    /// Resident (VK, host) pairs — the native analog of the multi-version
    /// ELF set shipped for a rollout window.
    hosts: Vec<(PredicateKey, NativeHost)>,
}

impl VersionedNativeAcctStrategy {
    pub(crate) fn new(hosts: Vec<(PredicateKey, NativeHost)>) -> Self {
        Self { hosts }
    }
}

impl ProveStrategy<AcctSpec> for VersionedNativeAcctStrategy {
    fn prove(
        &self,
        input: &<<AcctSpec as strata_paas::ProofSpec>::Program as ZkVmProgram>::Input,
        _ctx: ProveContext,
    ) -> ProverResult<ProofReceiptWithMetadata> {
        let host = match self.hosts.iter().find(|(vk, _)| *vk == input.update_vk) {
            Some((_, host)) => host,
            // AlwaysAccept verifies any witness, so any resident host will do.
            None if input.update_vk == PredicateKey::always_accept() => {
                let (_, host) = self.hosts.first().ok_or_else(|| {
                    ProverError::PermanentFailure("no prover hosts resident".to_string())
                })?;
                host
            }
            None => {
                return Err(ProverError::PermanentFailure(format!(
                    "no resident prover host for batch VK {:?}; the rollout for this \
                     version is not provisioned",
                    input.update_vk.id()
                )));
            }
        };

        <<AcctSpec as strata_paas::ProofSpec>::Program as ZkVmProgram>::prove(input, host)
            .map_err(|e| ProverError::PermanentFailure(e.to_string()))
    }
}
