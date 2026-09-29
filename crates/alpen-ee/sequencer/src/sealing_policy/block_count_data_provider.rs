//! Block-count data provider for [`max_value_policy`](super::max_value_policy).

use async_trait::async_trait;
use strata_acct_types::Hash;

use super::{max_value_policy::ValueAccumulatorPolicy, BlockDataProvider};

/// Reports `1` for every block, turning [`ValueAccumulatorPolicy`] into a block counter.
#[derive(Debug, Clone, Copy)]
pub struct BlockCountDataProvider;

#[async_trait]
impl BlockDataProvider<ValueAccumulatorPolicy> for BlockCountDataProvider {
    async fn get_block_data(&self, _hash: Hash) -> eyre::Result<Option<u64>> {
        Ok(Some(1))
    }
}
