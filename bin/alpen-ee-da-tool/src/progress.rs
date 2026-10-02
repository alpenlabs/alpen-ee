//! Terminal progress reporting for EE DA reconstruction.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use strata_identifiers::L1Height;
use strata_snark_acct_types::Seqno;

/// Reports progress for a stage without a meaningful item count.
pub(crate) struct StageProgress {
    bar: ProgressBar,
    stage: &'static str,
}

impl StageProgress {
    /// Creates a spinner for `stage`.
    pub(crate) fn new(stage: &'static str) -> Self {
        let style = ProgressStyle::with_template("{spinner} [{elapsed_precise}] {msg}")
            .expect("valid stage progress template");
        let bar = ProgressBar::new_spinner().with_style(style);
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message(format!("{stage} in progress"));
        Self { bar, stage }
    }

    /// Marks the stage complete.
    pub(crate) fn finish(self) {
        self.bar
            .finish_with_message(format!("{} complete", self.stage));
    }
}

impl Drop for StageProgress {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar
                .abandon_with_message(format!("{} stopped before completion", self.stage));
        }
    }
}

/// Reports progress while extracting EE DA from a bounded L1 block range.
pub(crate) struct EeDaExtractionProgress {
    bar: ProgressBar,
}

impl EeDaExtractionProgress {
    /// Creates an EE DA extraction progress bar for `block_count` L1 blocks.
    pub(crate) fn new(block_count: u64) -> Self {
        let style = ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )
        .expect("valid EE DA extraction progress template")
        .progress_chars("##-");
        let bar = ProgressBar::new(block_count).with_style(style);
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message("extracting EE DA from L1 blocks");
        Self { bar }
    }

    /// Records one L1 block successfully processed for EE DA.
    pub(crate) fn block_processed(&self, height: L1Height, total_recovered_blobs: usize) {
        self.bar.inc(1);
        self.bar.set_message(format!(
            "height {height}, recovered {total_recovered_blobs} DA blobs"
        ));
    }

    /// Completes EE DA extraction progress.
    pub(crate) fn finish(self, total_recovered_blobs: usize) {
        self.bar.finish_with_message(format!(
            "EE DA extraction complete, recovered {total_recovered_blobs} DA blobs"
        ));
    }
}

impl Drop for EeDaExtractionProgress {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar
                .abandon_with_message("EE DA extraction stopped before completion");
        }
    }
}

/// Reports progress while verifying reconstructed EE account updates against OL.
pub(crate) struct EeAccountVerificationProgress {
    bar: ProgressBar,
}

impl EeAccountVerificationProgress {
    /// Creates an EE account verification progress bar for `update_count` updates.
    pub(crate) fn new(update_count: usize) -> Self {
        let update_count =
            u64::try_from(update_count).expect("replayed update count must fit in u64");
        let style = ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )
        .expect("valid EE account verification progress template")
        .progress_chars("##-");
        let bar = ProgressBar::new(update_count).with_style(style);
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message("verifying reconstructed EE account state against OL");
        Self { bar }
    }

    /// Reports that the OL account update is being fetched.
    pub(crate) fn fetching_update(&self, update_seq_no: Seqno) {
        self.bar.set_message(format!(
            "fetching OL update sequence number {}",
            update_seq_no.inner()
        ));
    }

    /// Reports that the fetched account update is being verified.
    pub(crate) fn verifying_update(&self, update_seq_no: Seqno) {
        self.bar.set_message(format!(
            "verifying update sequence number {}",
            update_seq_no.inner()
        ));
    }

    /// Records one successfully verified account update.
    pub(crate) fn update_verified(&self, update_seq_no: Seqno) {
        self.bar.inc(1);
        self.bar.set_message(format!(
            "verified update sequence number {}",
            update_seq_no.inner()
        ));
    }

    /// Completes EE account verification progress.
    pub(crate) fn finish(self) {
        self.bar
            .finish_with_message("EE account state verification complete");
    }
}

impl Drop for EeAccountVerificationProgress {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar
                .abandon_with_message("EE account state verification stopped before completion");
        }
    }
}
