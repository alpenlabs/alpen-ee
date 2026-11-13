//! Alpen EE admin RPC handler implementation.

use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use alloy_primitives::B256;
use alpen_common::ChunkStatus;
use alpen_rpc_api::{AdminStatusResponse, AlpenAdminRpcServer, ChunkProofCoverageResponse};
use alpen_storage_api::ChunkStorage;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use reth_node_builder::NodeTypesWithDB;
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    ProviderResult,
};
use tracing::info;

use crate::{
    block_status::fetch_canonical_block_number,
    errors::{internal_error, invalid_params_error, not_sequencer_error},
    RpcBlockProductionControl,
};

/// Resolves a block hash to its height on the canonical chain.
pub trait CanonicalBlockNumber: Send + Sync {
    /// `Some(height)` when `hash` is the canonical block at that height,
    /// `None` when it is unknown or on a non-canonical branch.
    fn canonical_block_number(&self, hash: B256) -> ProviderResult<Option<u64>>;
}

impl<N: NodeTypesWithDB + ProviderNodeTypes> CanonicalBlockNumber for BlockchainProvider<N> {
    fn canonical_block_number(&self, hash: B256) -> ProviderResult<Option<u64>> {
        fetch_canonical_block_number(self, hash)
    }
}

/// The admin server's view of the canonical chain, filled once the execution
/// layer is up: the admin RPC starts before reth launches, so the lookup it
/// needs arrives later than the server does.
pub type CanonicalChainSlot = OnceLock<Arc<dyn CanonicalBlockNumber>>;

/// RPC handler for [`AlpenAdminRpcServer`].
#[derive(Clone)]
pub struct AdminRpcServer {
    version: &'static str,
    /// Present only on sequencers.
    block_production: Option<Arc<RpcBlockProductionControl>>,
    chunk_storage: Arc<dyn ChunkStorage>,
    canonical_chain: Arc<CanonicalChainSlot>,
}

impl fmt::Debug for AdminRpcServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminRpcServer")
            .field("version", &self.version)
            .field("sequencer", &self.block_production.is_some())
            .finish_non_exhaustive()
    }
}

impl AdminRpcServer {
    /// Creates an admin RPC handler reporting the given client version.
    ///
    /// `block_production` is the sequencer's block production control, or
    /// `None` on a full node. `canonical_chain` is filled by the node once
    /// the execution layer is launched; until then the chunk proof coverage
    /// method answers that the node is still starting.
    pub fn new(
        version: &'static str,
        block_production: Option<Arc<RpcBlockProductionControl>>,
        chunk_storage: Arc<dyn ChunkStorage>,
        canonical_chain: Arc<CanonicalChainSlot>,
    ) -> Self {
        Self {
            version,
            block_production,
            chunk_storage,
            canonical_chain,
        }
    }

    fn block_production(&self) -> RpcResult<&RpcBlockProductionControl> {
        self.block_production
            .as_deref()
            .ok_or_else(not_sequencer_error)
    }

    fn canonical_chain(&self) -> RpcResult<&dyn CanonicalBlockNumber> {
        self.canonical_chain
            .get()
            .map(Arc::as_ref)
            .ok_or_else(|| internal_error("execution layer is still starting"))
    }

    fn canonical_block_number(&self, hash: &[u8]) -> RpcResult<Option<u64>> {
        self.canonical_chain()?
            .canonical_block_number(B256::from_slice(hash))
            .map_err(|e| internal_error(e.to_string()))
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

    async fn get_chunk_proof_coverage(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> RpcResult<ChunkProofCoverageResponse> {
        if start_block == 0 || start_block > end_block {
            return Err(invalid_params_error(
                "start_block must be non-zero and less than or equal to end_block",
            ));
        }

        let latest_chunk = self
            .chunk_storage
            .get_latest_chunk()
            .await
            .map_err(|e| internal_error(e.to_string()))?;

        let Some((latest_chunk, _)) = latest_chunk else {
            return Ok(ChunkProofCoverageResponse {
                start_block,
                end_block,
                covered: false,
                first_uncovered_block: Some(start_block),
            });
        };

        let mut first_uncovered_block = start_block;

        for chunk_idx in 0..=latest_chunk.idx() {
            let Some((chunk, status)) = self
                .chunk_storage
                .get_chunk_by_idx(chunk_idx)
                .await
                .map_err(|e| internal_error(e.to_string()))?
            else {
                continue;
            };

            let Some(prev_block_num) =
                self.canonical_block_number(chunk.prev_block().as_slice())?
            else {
                continue;
            };
            let Some(last_block_num) =
                self.canonical_block_number(chunk.last_block().as_slice())?
            else {
                continue;
            };

            let chunk_start_block = prev_block_num.saturating_add(1);
            if last_block_num < first_uncovered_block {
                continue;
            }
            if chunk_start_block > end_block {
                break;
            }
            if chunk_start_block > first_uncovered_block
                || !matches!(status, ChunkStatus::ProofReady(_))
            {
                continue;
            }
            if last_block_num >= end_block {
                return Ok(ChunkProofCoverageResponse {
                    start_block,
                    end_block,
                    covered: true,
                    first_uncovered_block: None,
                });
            }
            first_uncovered_block = last_block_num + 1;
        }

        Ok(ChunkProofCoverageResponse {
            start_block,
            end_block,
            covered: false,
            first_uncovered_block: Some(first_uncovered_block),
        })
    }
}

#[cfg(test)]
mod tests {
    use alpen_storage_api::testing::InMemoryStorage;

    use super::*;

    fn test_server(block_production: Option<Arc<RpcBlockProductionControl>>) -> AdminRpcServer {
        AdminRpcServer::new(
            "1.2.3",
            block_production,
            Arc::new(InMemoryStorage::new_empty()),
            Arc::new(CanonicalChainSlot::new()),
        )
    }

    fn sequencer_server() -> AdminRpcServer {
        test_server(Some(Arc::default()))
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
        let server = test_server(None);
        assert!(!server.get_admin_status().await.unwrap().sequencer);
        assert!(server.start_block_production().await.is_err());
        assert!(server.stop_block_production(None).await.is_err());
    }
}
