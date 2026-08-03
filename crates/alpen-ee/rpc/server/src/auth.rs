//! JWT-authenticated RPC server launcher.
//!
//! Reuses reth's engine-API auth stack ([`AuthLayer`] + [`JwtAuthValidator`])
//! to serve RPC modules on a dedicated port. Requests must carry an
//! `Authorization: Bearer <jwt>` header signed with the shared secret; the
//! layer rejects everything else with `401 Unauthorized` before it reaches
//! the RPC service.

use std::{io, net::SocketAddr, path::Path};

use jsonrpsee::{
    server::{ServerBuilder, ServerHandle},
    Methods,
};
use reth_rpc_layer::{AuthLayer, JwtAuthValidator, JwtError, JwtSecret};
use tower::ServiceBuilder;

/// Reads the JWT secret hex file at `path`, generating and persisting a new
/// random secret when the file does not exist.
///
/// Mirrors reth's `get_or_create_jwt_secret_from_path` used for the engine
/// API default secret.
pub fn get_or_create_jwt_secret(path: &Path) -> Result<JwtSecret, JwtError> {
    if path.exists() {
        JwtSecret::from_file(path)
    } else {
        JwtSecret::try_create_random(path)
    }
}

/// Starts a JWT-authenticated jsonrpsee server for `methods` on `addr`.
///
/// Returns the bound address (useful when `addr` requests an ephemeral port)
/// and the running server's handle. The server stops when the handle is
/// dropped.
pub async fn start_authenticated_rpc_server(
    addr: SocketAddr,
    secret: JwtSecret,
    methods: impl Into<Methods>,
) -> io::Result<(SocketAddr, ServerHandle)> {
    let middleware = ServiceBuilder::new().layer(AuthLayer::new(JwtAuthValidator::new(secret)));
    let server = ServerBuilder::new()
        .set_http_middleware(middleware)
        .build(addr)
        .await?;
    let local_addr = server.local_addr()?;
    Ok((local_addr, server.start(methods)))
}

#[cfg(test)]
mod tests {
    use alpen_ee_rpc_api::{AlpenAdminRpcClient, AlpenAdminRpcServer as _};
    use http::{header::AUTHORIZATION, HeaderMap};
    use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
    use reth_rpc_layer::secret_to_bearer_header;

    use super::*;
    use crate::AdminRpcServer;

    async fn spawn_admin_server(secret: JwtSecret) -> (SocketAddr, ServerHandle) {
        let module = AdminRpcServer::new("test-version", false).into_rpc();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        start_authenticated_rpc_server(addr, secret, module)
            .await
            .unwrap()
    }

    fn client_with_secret(addr: SocketAddr, secret: &JwtSecret) -> HttpClient {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, secret_to_bearer_header(secret));
        HttpClientBuilder::default()
            .set_headers(headers)
            .build(format!("http://{addr}"))
            .unwrap()
    }

    #[tokio::test]
    async fn authenticated_request_succeeds() {
        let secret = JwtSecret::random();
        let (addr, _handle) = spawn_admin_server(secret).await;

        let status = client_with_secret(addr, &secret)
            .get_admin_status()
            .await
            .unwrap();
        assert_eq!(status.version, "test-version");
        assert!(!status.sequencer);
    }

    #[tokio::test]
    async fn missing_token_is_rejected() {
        let secret = JwtSecret::random();
        let (addr, _handle) = spawn_admin_server(secret).await;

        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();
        let err = client.get_admin_status().await.unwrap_err();
        assert!(err.to_string().contains("401"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected() {
        let secret = JwtSecret::random();
        let (addr, _handle) = spawn_admin_server(secret).await;

        let err = client_with_secret(addr, &JwtSecret::random())
            .get_admin_status()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"), "unexpected error: {err}");
    }

    #[test]
    fn get_or_create_jwt_secret_creates_then_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("admin-jwt.hex");

        let created = get_or_create_jwt_secret(&path).unwrap();
        assert!(path.exists());

        let read_back = get_or_create_jwt_secret(&path).unwrap();
        assert_eq!(created, read_back);
    }
}
