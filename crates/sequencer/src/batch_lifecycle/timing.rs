//! Wall-clock timing of each batch's lifecycle stages, for metrics.
//!
//! The lifecycle only persists statuses, not when they changed, so this
//! in-memory clock keeps, per batch, when the lifecycle first saw it sealed
//! and when it last changed stage. Both are lost on restart: a batch that
//! resumes mid-lifecycle simply reports no stage durations.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use metrics::{counter, histogram};

use crate::metrics::{BATCH_SETTLEMENT_SECONDS, BATCH_STATUS_SECONDS, BATCH_TRANSITIONS};

/// The stage a batch enters when its proof is ready; the clock forgets the
/// batch after it.
const FINAL_STAGE: &str = "proof_ready";

/// When a batch was first seen and when it last changed stage.
#[derive(Debug, Clone, Copy)]
struct BatchTimes {
    first_seen: Instant,
    stage_entered: Instant,
}

/// Per-batch stage timing, keyed by batch index.
#[derive(Debug, Default)]
pub(crate) struct StageClock {
    batches: HashMap<u64, BatchTimes>,
}

impl StageClock {
    /// Notes that the lifecycle sees `idx` as sealed, if it had not already.
    pub(crate) fn observe(&mut self, idx: u64) {
        self.batches.entry(idx).or_insert_with(|| {
            let now = Instant::now();
            BatchTimes {
                first_seen: now,
                stage_entered: now,
            }
        });
    }

    /// Records that batch `idx` entered `stage`: counts the transition and,
    /// when the batch's timing is known, records how long the previous stage
    /// took. Entering the final stage also records the whole settlement time
    /// and drops the batch.
    pub(crate) fn transition(&mut self, idx: u64, stage: &'static str) {
        counter!(BATCH_TRANSITIONS, "status" => stage).increment(1);
        let now = Instant::now();
        let Some(times) = self.batches.get_mut(&idx) else {
            return;
        };
        histogram!(BATCH_STATUS_SECONDS, "status" => stage)
            .record(secs(now.duration_since(times.stage_entered)));
        times.stage_entered = now;
        if stage == FINAL_STAGE {
            histogram!(BATCH_SETTLEMENT_SECONDS).record(secs(now.duration_since(times.first_seen)));
            self.batches.remove(&idx);
        }
    }

    /// Forgets every batch, as after a reorg reset.
    pub(crate) fn clear(&mut self) {
        self.batches.clear();
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}
