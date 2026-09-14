//! Alpen EE admin RPC handler implementation.

use alpen_ee_rpc_api::{AdminStatusResponse, AlpenAdminRpcServer};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;

/// RPC handler for [`AlpenAdminRpcServer`].
#[derive(Debug, Clone)]
pub struct AdminRpcServer {
    version: &'static str,
    sequencer: bool,
}

impl AdminRpcServer {
    /// Creates an admin RPC handler reporting the given client version and
    /// sequencer mode.
    pub fn new(version: &'static str, sequencer: bool) -> Self {
        Self { version, sequencer }
    }
}

#[async_trait]
impl AlpenAdminRpcServer for AdminRpcServer {
    async fn get_admin_status(&self) -> RpcResult<AdminStatusResponse> {
        Ok(AdminStatusResponse {
            version: self.version.to_string(),
            sequencer: self.sequencer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_admin_status_reports_constructor_values() {
        let server = AdminRpcServer::new("1.2.3", true);
        let status = server.get_admin_status().await.unwrap();
        assert_eq!(
            status,
            AdminStatusResponse {
                version: "1.2.3".to_string(),
                sequencer: true,
            }
        );
    }
}
