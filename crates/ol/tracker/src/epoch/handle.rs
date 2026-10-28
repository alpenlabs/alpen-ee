use std::{sync::Arc, time::Duration};

use alpen_common::{ConsensusHeads, Storage};
use alpen_ol_api::OLClient;
use strata_service::{
    AsyncExecutor, DumbTickHandle, DumbTickingInput, ServiceBuilder, ServiceMonitor,
};
use tokio::sync::watch;
use tracing::warn;

use crate::{
    epoch::{
        EpochTrackingMode, OLTrackerService, OLTrackerServiceState, OLTrackerState, OLTrackerStatus,
    },
    OLFinalizedStatus,
};

/// Default number of OL epochs to process in each cycle.
const DEFAULT_MAX_EPOCHS_FETCH: u32 = 10;
/// Default ms to wait between OL polls.
const DEFAULT_POLL_WAIT_MS: u64 = 1_000;

/// Handle for accessing OL tracker state updates.
#[derive(Debug)]
pub struct OLTrackerHandle {
    ol_status_rx: watch::Receiver<OLFinalizedStatus>,
    consensus_rx: watch::Receiver<ConsensusHeads>,
    monitor: ServiceMonitor<OLTrackerStatus>,
    tick_handle: DumbTickHandle,
}

impl OLTrackerHandle {
    /// Returns a watcher for OL finalized status updates.
    pub fn ol_status_watcher(&self) -> watch::Receiver<OLFinalizedStatus> {
        self.ol_status_rx.clone()
    }

    /// Returns a watcher for consensus head updates.
    pub fn consensus_watcher(&self) -> watch::Receiver<ConsensusHeads> {
        self.consensus_rx.clone()
    }

    /// Returns the service framework monitor.
    pub fn monitor(&self) -> &ServiceMonitor<OLTrackerStatus> {
        &self.monitor
    }

    /// Stops the OL tracker service.
    ///
    /// Returns false if the service was already stopping.
    pub fn stop(self) -> bool {
        self.tick_handle.stop()
    }
}

/// Starts the OL tracker as a service framework service.
///
/// Uses default polling interval and max epochs per cycle.
pub async fn start_ol_tracker_service<TStorage, TOLClient>(
    state: OLTrackerState,
    genesis_epoch: u32,
    storage: Arc<TStorage>,
    ol_client: Arc<TOLClient>,
    tracking_mode: EpochTrackingMode,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<OLTrackerHandle>
where
    TStorage: Storage + 'static,
    TOLClient: OLClient + 'static,
{
    start_ol_tracker_service_with(
        state,
        genesis_epoch,
        storage,
        ol_client,
        tracking_mode,
        DEFAULT_POLL_WAIT_MS,
        DEFAULT_MAX_EPOCHS_FETCH,
        executor,
    )
    .await
}

/// Starts the OL tracker as a service with custom configuration.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper with distinct config params"
)]
async fn start_ol_tracker_service_with<TStorage, TOLClient>(
    state: OLTrackerState,
    genesis_epoch: u32,
    storage: Arc<TStorage>,
    ol_client: Arc<TOLClient>,
    tracking_mode: EpochTrackingMode,
    poll_wait_ms: u64,
    max_epochs_fetch: u32,
    executor: &impl AsyncExecutor,
) -> anyhow::Result<OLTrackerHandle>
where
    TStorage: Storage + 'static,
    TOLClient: OLClient + 'static,
{
    // Initialize watch channels from current state.
    let (ol_status_tx, ol_status_rx) = watch::channel(state.get_ol_status());
    let (consensus_tx, consensus_rx) = watch::channel(state.get_consensus_heads());

    if tracking_mode == EpochTrackingMode::Latest {
        warn!(
            component = "alpen",
            "ol_tracker: Tracking latest epoch. This should only be used for testing"
        );
    }

    let service_state = OLTrackerServiceState {
        storage,
        ol_client,
        genesis_epoch,
        max_epochs_fetch,
        tracking_mode,
        ol_status_tx,
        consensus_tx,
        tracker_state: state,
    };

    let (tick_handle, tick_input) = DumbTickingInput::new(Duration::from_millis(poll_wait_ms));

    let monitor = ServiceBuilder::<OLTrackerService<_, _>, _>::new()
        .with_state(service_state)
        .with_input(tick_input)
        .launch_async("ol_tracker", executor)
        .await?;

    Ok(OLTrackerHandle {
        ol_status_rx,
        consensus_rx,
        monitor,
        tick_handle,
    })
}
