use async_trait::async_trait;

use crate::{ExecutionEngine, PayloadBuildAttributes};

#[async_trait]
pub trait PayloadBuilderEngine: ExecutionEngine {
    async fn build_payload(
        &self,
        build_attrs: PayloadBuildAttributes,
    ) -> eyre::Result<Self::TEnginePayload>;
}
