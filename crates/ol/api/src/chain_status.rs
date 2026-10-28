use strata_identifiers::{EpochCommitment, OLBlockCommitment};

/// Status of the OL chain including tip, confirmed and finalized epochs.
#[derive(Debug, Clone, Copy)]
pub struct OLChainStatus {
    /// Tip block commitment.
    pub tip: OLBlockCommitment,

    /// Confirmed epoch commitment.
    pub confirmed: EpochCommitment,

    /// Finalized epoch commitment.
    pub finalized: EpochCommitment,

    /// Latest epoch terminal block processed by the OL fork-choice manager.
    pub latest: EpochCommitment,
}

impl OLChainStatus {
    /// Returns the tip block commitment.
    pub fn tip(&self) -> &OLBlockCommitment {
        &self.tip
    }

    /// Returns the confirmed epoch commitment.
    pub fn confirmed(&self) -> &EpochCommitment {
        &self.confirmed
    }

    /// Returns the finalized epoch commitment.
    pub fn finalized(&self) -> &EpochCommitment {
        &self.finalized
    }

    /// Returns the latest completed OL epoch.
    pub fn latest(&self) -> &EpochCommitment {
        &self.latest
    }
}
