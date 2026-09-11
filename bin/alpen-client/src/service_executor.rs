use std::future::Future;

use reth_tasks::{shutdown::GracefulShutdown, TaskExecutor};
use strata_service::{AsyncExecutor, AsyncGuard};
use tracing::{info_span, Instrument};

pub(crate) struct ServiceExecutor {
    inner: TaskExecutor,
}

impl ServiceExecutor {
    pub(crate) fn from_reth(inner: TaskExecutor) -> Self {
        Self { inner }
    }
}

impl AsyncExecutor for ServiceExecutor {
    type ShutdownGuard = ServiceShutdownGuard;

    fn spawn_async<F>(
        &self,
        name: &'static str,
        worker: impl FnOnce(Self::ShutdownGuard) -> F + Send + 'static,
    ) where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let span = info_span!("alpen_service", component = "alpen");
        // The worker owns its shutdown: it observes the signal through the guard and runs its
        // own shutdown hooks, and the runtime's graceful shutdown waits for it to finish
        // (bounded by the runner's timeout).
        self.inner
            .spawn_critical_with_graceful_shutdown_signal(name, |shutdown| {
                async move {
                    worker(ServiceShutdownGuard(shutdown))
                        .await
                        .expect("critical service should not error")
                }
                .instrument(span)
            });
    }
}

pub(crate) struct ServiceShutdownGuard(GracefulShutdown);

impl AsyncGuard for ServiceShutdownGuard {
    fn wait_for_shutdown(&self) -> impl Future<Output = ()> {
        self.0.clone().ignore_guard()
    }
}
