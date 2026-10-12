//! Alpen EE admin RPC handler implementation.

use std::sync::Arc;

use alpen_rpc_api::{AdminStatusResponse, AlpenAdminRpcServer};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use tracing::info;

use crate::{errors::not_sequencer_error, RpcBlockProductionControl};

/// RPC handler for [`AlpenAdminRpcServer`].
#[derive(Debug, Clone)]
pub struct AdminRpcServer {
    version: &'static str,
    /// Present only on sequencers.
    block_production: Option<Arc<RpcBlockProductionControl>>,
}

impl AdminRpcServer {
    /// Creates an admin RPC handler reporting the given client version.
    ///
    /// `block_production` is the sequencer's block production control, or
    /// `None` on a full node.
    pub fn new(
        version: &'static str,
        block_production: Option<Arc<RpcBlockProductionControl>>,
    ) -> Self {
        Self {
            version,
            block_production,
        }
    }

    fn block_production(&self) -> RpcResult<&RpcBlockProductionControl> {
        self.block_production
            .as_deref()
            .ok_or_else(not_sequencer_error)
    }
}

#[async_trait]
impl AlpenAdminRpcServer for AdminRpcServer {
    async fn get_admin_status(&self) -> RpcResult<AdminStatusResponse> {
        Ok(AdminStatusResponse {
            version: self.version.to_string(),
            sequencer: self.block_production.is_some(),
            block_production_stop_after: self
                .block_production
                .as_ref()
                .and_then(|control| control.stop_after_blocknum()),
        })
    }

    async fn start_block_production(&self) -> RpcResult<()> {
        self.block_production()?.clear();
        info!("block production started via admin RPC");
        Ok(())
    }

    async fn stop_block_production(&self, after_block: Option<u64>) -> RpcResult<()> {
        // 0 is at or below every tip, so production stops immediately.
        self.block_production()?
            .stop_after(after_block.unwrap_or(0));
        info!(?after_block, "block production stopped via admin RPC");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequencer_server() -> AdminRpcServer {
        AdminRpcServer::new("1.2.3", Some(Arc::default()))
    }

    #[tokio::test]
    async fn get_admin_status_reports_constructor_values() {
        let server = sequencer_server();
        let status = server.get_admin_status().await.unwrap();
        assert_eq!(
            status,
            AdminStatusResponse {
                version: "1.2.3".to_string(),
                sequencer: true,
                block_production_stop_after: None,
            }
        );
    }

    #[tokio::test]
    async fn stop_without_block_stops_immediately() {
        let server = sequencer_server();
        server.stop_block_production(None).await.unwrap();
        let status = server.get_admin_status().await.unwrap();
        assert_eq!(status.block_production_stop_after, Some(0));
    }

    #[tokio::test]
    async fn stop_after_block_then_start_clears_limit() {
        let server = sequencer_server();
        server.stop_block_production(Some(10)).await.unwrap();
        let status = server.get_admin_status().await.unwrap();
        assert_eq!(status.block_production_stop_after, Some(10));

        server.start_block_production().await.unwrap();
        let status = server.get_admin_status().await.unwrap();
        assert_eq!(status.block_production_stop_after, None);
    }

    #[tokio::test]
    async fn full_node_rejects_block_production_control() {
        let server = AdminRpcServer::new("1.2.3", None);
        assert!(!server.get_admin_status().await.unwrap().sequencer);
        assert!(server.start_block_production().await.is_err());
        assert!(server.stop_block_production(None).await.is_err());
    }
}
