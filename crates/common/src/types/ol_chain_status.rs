use strata_acct_types::Hash;
use strata_identifiers::OLBlockCommitment;

/// Finalized OL block and its corresponding EE block hash.
#[derive(Debug, Clone, Copy)]
pub struct OLFinalizedStatus {
    /// finalized ol block.
    pub ol_block: OLBlockCommitment,
    /// blockhash of last ee block whose update was posted upto this ol block.
    pub last_ee_block: Hash,
}
