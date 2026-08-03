//! Embedded EVM chain spec, materialized per protocol spec version.

use std::sync::Arc;

use alloy_genesis::Genesis;
use reth_chainspec::{ChainSpec, EthereumHardfork, ForkCondition};
use serde::{Deserialize, Serialize, Serializer};

use crate::{
    genesis_info::{ee_genesis_block_info, AlpenEeGenesisBlockInfo},
    spec_activations::known_versions,
    AlpenSpecId,
};

/// The embedded EVM chain spec: genesis document plus the derived reth chain
/// spec of every protocol spec version.
///
/// The JSON form is the standard EVM genesis document (chain config plus
/// allocation), exactly what `--custom-chain` used to load as a separate
/// file. Decoding eagerly derives one reth [`ChainSpec`] per [`AlpenSpecId`]:
/// [`AlpenSpecId::V0`]'s comes from the genesis document as-is, and each
/// successor's is its predecessor's with that version's code-owned delta
/// applied on top. Every consumer reads the same values and no boot-time
/// genesis cross-check is needed — agreement is structural.
///
/// Validity of the document is reth's concern, not ours: decoding accepts
/// exactly what reth's `Genesis -> ChainSpec` conversion accepts and derives
/// the specs from it, rather than re-policing individual genesis fields
/// (which would only diverge from — and lag behind — reth's own semantics).
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "Genesis")]
pub struct EvmSpec {
    /// The parsed genesis document, authoritative for serialization.
    genesis: Genesis,

    /// Chain spec of each known [`AlpenSpecId`], indexed by discriminant;
    /// never serialized.
    ///
    /// Entries are `Arc<ChainSpec>` — not because [`EvmSpec`] needs shared
    /// ownership, but to match reth's boundary: the node's `command.chain`
    /// field is `Arc<ChainSpec>`, so consumers hand one off with a cheap
    /// refcount bump rather than deep-cloning the spec (or re-deriving it
    /// from `genesis`) on every use.
    chain_specs: Vec<Arc<ChainSpec>>,
}

impl EvmSpec {
    /// Returns the genesis document.
    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// Returns the derived reth chain spec of `version`.
    ///
    /// Total over the closed [`AlpenSpecId`] enum, whether or not the
    /// schedule has activated the version: what a version *means* is static
    /// and derivable for all of them; *when* its spec governs is the
    /// schedule's concern, not this table's.
    pub fn chain_spec(&self, version: AlpenSpecId) -> &Arc<ChainSpec> {
        self.chain_specs
            .get(usize::from(u16::from(version)))
            .expect("EvmSpec invariant: the table covers every known version")
    }

    /// Returns the chain spec of every known version, indexed by
    /// discriminant — the whole table behind [`EvmSpec::chain_spec`], for
    /// consumers that resolve versions per block rather than fixing one.
    pub fn chain_specs(&self) -> &[Arc<ChainSpec>] {
        &self.chain_specs
    }

    /// Returns the genesis block facts, derived from the chain spec on demand.
    ///
    /// Served from [`AlpenSpecId::V0`]'s spec but version-invariant: deltas
    /// never touch the genesis identity.
    pub fn genesis_info(&self) -> AlpenEeGenesisBlockInfo {
        ee_genesis_block_info(self.chain_spec(AlpenSpecId::V0))
    }
}

/// Derives the chain spec of every known spec version, indexed by
/// discriminant.
///
/// `base` is [`AlpenSpecId::V0`]'s spec, derived from the genesis document;
/// each successor's spec is a clone of its predecessor's with the successor's
/// delta applied on top. Deltas mutate the clone in place and never pass back
/// through reth's `Genesis -> ChainSpec` derivation: reth bakes the
/// active-at-genesis fork set into the genesis header at construction, so
/// re-deriving with a version's wider fork set would silently mint a
/// different genesis identity (pinned by tests).
fn derive_chain_specs(base: Arc<ChainSpec>) -> Vec<Arc<ChainSpec>> {
    let mut chain_specs = vec![base];
    for version in known_versions().skip(1) {
        let mut spec = (**chain_specs.last().expect("v0 seeds the fold")).clone();
        apply_evm_delta(version, &mut spec);
        chain_specs.push(Arc::new(spec));
    }
    chain_specs
}

/// Applies the EVM chain-spec delta that `version` introduces on top of its
/// predecessor's spec.
///
/// This mapping is code, not artifact data: the params artifact schedules
/// *when* a version activates, while what it means for the EVM is baked into
/// the binary — the standard EVM model, and what keeps
/// [`UnknownSuccessor`](crate::AlpenSpecScheduleError::UnknownSuccessor) a
/// real safety property (a params file cannot redefine an upgrade's
/// semantics for an old binary). Fork conditions set here must be
/// unconditional (active from genesis): version selection happens at the
/// Alpen layer by activation coordinate, so within one version's spec there
/// is no boundary.
///
/// When a delta first enables a fork with transition semantics (initial
/// base fee, blob-field initialization), that boundary handling must live
/// where the resolver switches specs — a version's own spec has the fork
/// active from genesis and never sees the transition.
fn apply_evm_delta(version: AlpenSpecId, chain_spec: &mut ChainSpec) {
    match version {
        AlpenSpecId::V0 => unreachable!("v0 is the fold's seed, not a delta"),
        // Osaka has no transition semantics to handle at the version
        // boundary: it adds no header fields (blob fields exist since
        // Cancun, requests_hash since Prague), and its blob params were
        // already populated by v0's genesis derivation — resolution keys on
        // the active fork, not the fork set the params were derived under.
        AlpenSpecId::V1 => {
            chain_spec
                .hardforks
                .insert(EthereumHardfork::Osaka, ForkCondition::Timestamp(0));
        }
    }
}

impl From<Genesis> for EvmSpec {
    fn from(genesis: Genesis) -> Self {
        let base: Arc<ChainSpec> = Arc::new(genesis.clone().into());
        let chain_specs = derive_chain_specs(base);
        Self {
            genesis,
            chain_specs,
        }
    }
}

// Manual instead of derived: `ChainSpec` has no `Default`, so `chain_spec`
// must be derived from `genesis` the same way `From<Genesis>` does.
impl Default for EvmSpec {
    /// An empty genesis document (chain id 0, no allocation), i.e. what
    /// reth's `Genesis -> ChainSpec` conversion derives from `{}`: every
    /// hardfork active from genesis. Not a valid spec for any real network;
    /// intended for tests and benchmarks that don't exercise EVM execution.
    fn default() -> Self {
        Genesis::default().into()
    }
}

impl Serialize for EvmSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.genesis.serialize(serializer)
    }
}

// Compare the genesis document only: `ChainSpec`'s derived equality goes
// through `SealedHeader`'s lazily initialized hash cache and is therefore
// initialization-order-sensitive, and everything else here is derived from
// `genesis` anyway.
impl PartialEq for EvmSpec {
    fn eq(&self, other: &Self) -> bool {
        self.genesis == other.genesis
    }
}

impl Eq for EvmSpec {}

#[cfg(test)]
mod tests {
    use alpen_chainspec::DEV_CHAIN_SPEC;
    use reth_chainspec::{EthereumHardfork, EthereumHardforks, ForkCondition};

    use super::EvmSpec;
    use crate::{
        genesis_info::ee_genesis_block_info_from_json, spec_activations::known_versions,
        AlpenSpecId,
    };

    #[test]
    fn json_roundtrip_preserves_evm_spec() {
        let spec: EvmSpec = serde_json::from_str(DEV_CHAIN_SPEC).expect("dev chain should parse");

        let json = serde_json::to_string(&spec).expect("evm spec should serialize");
        let decoded: EvmSpec = serde_json::from_str(&json).expect("evm spec should reparse");

        assert_eq!(decoded, spec);
        assert_eq!(decoded.genesis_info(), spec.genesis_info());
    }

    #[test]
    fn genesis_info_matches_chainspec_derivation() {
        let spec: EvmSpec = serde_json::from_str(DEV_CHAIN_SPEC).expect("dev chain should parse");
        let expected =
            ee_genesis_block_info_from_json(DEV_CHAIN_SPEC).expect("dev chain should parse");

        assert_eq!(spec.genesis_info(), expected);
        assert_eq!(
            spec.chain_spec(AlpenSpecId::V0).genesis_hash(),
            expected.blockhash()
        );
    }

    #[test]
    fn json_accepts_what_reth_accepts() {
        // Validity is deferred to reth's `Genesis -> ChainSpec` conversion, so
        // even a minimal document decodes and derives genesis info without
        // policing individual fields (or panicking on an absent block number).
        let spec: EvmSpec = serde_json::from_str("{}").expect("empty genesis is accepted");
        let _ = spec.genesis_info();
    }

    #[test]
    fn default_matches_empty_genesis_json() {
        let from_json: EvmSpec = serde_json::from_str("{}").expect("empty genesis is accepted");
        assert_eq!(EvmSpec::default(), from_json);
    }

    /// The load-bearing invariant of the per-version table: deltas change
    /// rules, never the chain's genesis identity.
    #[test]
    fn every_version_shares_the_genesis_identity() {
        let spec: EvmSpec = serde_json::from_str(DEV_CHAIN_SPEC).expect("dev chain should parse");
        let v0 = spec.chain_spec(AlpenSpecId::V0);

        for version in known_versions().skip(1) {
            let versioned = spec.chain_spec(version);
            assert_eq!(versioned.genesis_hash(), v0.genesis_hash(), "{version:?}");
            assert_eq!(
                versioned.genesis_header(),
                v0.genesis_header(),
                "{version:?}"
            );
        }
    }

    /// Pins v1's delta: Osaka governs v1's spec from genesis and is absent
    /// from v0's.
    #[test]
    fn v1_activates_osaka_from_genesis() {
        let spec: EvmSpec = serde_json::from_str(DEV_CHAIN_SPEC).expect("dev chain should parse");

        assert!(!spec
            .chain_spec(AlpenSpecId::V0)
            .is_osaka_active_at_timestamp(0));
        assert!(spec
            .chain_spec(AlpenSpecId::V1)
            .is_osaka_active_at_timestamp(0));
    }

    /// Pins the property the fold relies on: reth bakes the
    /// active-at-genesis fork set into the genesis header, so a version's
    /// delta must mutate a clone of its predecessor's spec — re-deriving
    /// from the genesis document with the wider fork set would mint a
    /// different genesis identity.
    #[test]
    fn deltas_must_clone_not_rederive() {
        let plain: EvmSpec = serde_json::from_str("{}").expect("empty genesis is accepted");
        let plain_v0 = plain.chain_spec(AlpenSpecId::V0);

        // The same fork set passed through genesis derivation changes the
        // genesis identity...
        let cancun: EvmSpec = serde_json::from_str(r#"{"config":{"cancunTime":0}}"#)
            .expect("cancun genesis is accepted");
        assert_ne!(
            cancun.chain_spec(AlpenSpecId::V0).genesis_hash(),
            plain_v0.genesis_hash()
        );

        // ...while a delta-style in-place mutation of a clone does not.
        let mut mutated = (**plain_v0).clone();
        mutated
            .hardforks
            .insert(EthereumHardfork::Cancun, ForkCondition::Timestamp(0));
        assert_eq!(mutated.genesis_hash(), plain_v0.genesis_hash());
        assert_eq!(mutated.genesis_header(), plain_v0.genesis_header());
    }
}
