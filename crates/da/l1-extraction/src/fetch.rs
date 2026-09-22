//! Streams Bitcoin blocks from bitcoind with bounded concurrency and retry.

use std::{future::Future, num::NonZeroUsize, time::Duration};

use bitcoin::Block;
use bitcoind_async_client::{error::ClientError, traits::Reader, ClientResult};
use futures::{stream, Stream, StreamExt};
use strata_btc_types::BlockHashExt;
use strata_common::retry::{policies::ExponentialBackoff, Backoff};
use strata_identifiers::{L1BlockId, L1Height};
use thiserror::Error;
use tokio::time::sleep;
use tracing::warn;

/// bitcoind RPC error code returned while the node is warming up.
const BITCOIND_RPC_WARMUP: i32 = -28;

/// Raw Bitcoin block data and height.
#[derive(Debug, Clone)]
pub struct L1BlockData {
    height: L1Height,
    block: Block,
}

impl L1BlockData {
    /// Constructs L1 block data.
    pub fn new(height: L1Height, block: Block) -> Self {
        Self { height, block }
    }

    /// Returns the L1 block height.
    pub fn height(&self) -> L1Height {
        self.height
    }

    /// Returns the L1 block id.
    pub fn block_id(&self) -> L1BlockId {
        self.block.block_hash().to_l1_block_id()
    }

    /// Returns the Bitcoin block.
    pub fn block(&self) -> &Block {
        &self.block
    }
}

/// Failure to construct an L1 block range stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum FetchRangeError {
    /// L1 height range is inverted (start > end).
    #[error("start height {start_height} must be <= end height {end_height}")]
    Inverted {
        start_height: L1Height,
        end_height: L1Height,
    },
}

/// Retry policy for fetching an L1 block.
#[derive(Debug)]
pub struct FetchRetryPolicy {
    max_retries: u16,
    backoff: ExponentialBackoff,
}

impl FetchRetryPolicy {
    /// Creates an L1 block fetch retry policy.
    pub fn new(max_retries: u16, backoff: ExponentialBackoff) -> Self {
        Self {
            max_retries,
            backoff,
        }
    }

    /// Returns the maximum number of retries per block fetch.
    pub fn max_retries(&self) -> u16 {
        self.max_retries
    }

    /// Returns the retry backoff policy.
    pub fn backoff(&self) -> &ExponentialBackoff {
        &self.backoff
    }
}

/// Policy for fetching L1 blocks.
///
/// Peak buffering memory grows approximately linearly with
/// `max_concurrent_blocks` because each in-flight fetch may retain one decoded
/// Bitcoin block.
#[derive(Debug)]
pub struct FetchPolicy {
    retry: FetchRetryPolicy,
    max_concurrent_blocks: NonZeroUsize,
}

impl FetchPolicy {
    /// Creates an L1 block fetch policy.
    pub fn new(retry: FetchRetryPolicy, max_concurrent_blocks: NonZeroUsize) -> Self {
        Self {
            retry,
            max_concurrent_blocks,
        }
    }

    /// Returns the block fetch retry policy.
    pub fn retry(&self) -> &FetchRetryPolicy {
        &self.retry
    }

    /// Returns the maximum number of block fetches in flight.
    pub fn max_concurrent_blocks(&self) -> NonZeroUsize {
        self.max_concurrent_blocks
    }
}

/// Failure to fetch an L1 block.
#[derive(Debug, Error)]
pub enum FetchBlockError {
    /// A transient client failure persisted through the retry budget.
    #[error(
        "L1 block fetch retries exhausted (height {height}, max retries {max_retries}): {source}"
    )]
    RetriesExhausted {
        height: L1Height,
        max_retries: u16,
        #[source]
        source: ClientError,
    },

    /// A non-retryable client failure occurred.
    #[error("L1 block fetch failed at height {height}: {source}")]
    Client {
        height: L1Height,
        #[source]
        source: ClientError,
    },
}

/// Narrow adapter seam over the bitcoind block reader method used by fetch.
///
/// This trait exists to unit-test fetch retry behavior without depending on a
/// live bitcoind instance. It is not intended as a broad extension API.
pub trait L1BlockFetcher: Send + Sync {
    /// Fetches the block at `height`.
    fn get_block_at(
        &self,
        height: L1Height,
    ) -> impl Future<Output = ClientResult<Block>> + Send + '_;
}

impl<T> L1BlockFetcher for T
where
    T: Reader + Send + Sync,
{
    fn get_block_at(
        &self,
        height: L1Height,
    ) -> impl Future<Output = ClientResult<Block>> + Send + '_ {
        Reader::get_block_at(self, u64::from(height))
    }
}

/// Returns a height-ordered stream of Bitcoin blocks over an inclusive range.
///
/// The stream fetches up to [`FetchPolicy::max_concurrent_blocks`] blocks
/// concurrently while yielding results in ascending height order. A retrying
/// earlier height therefore delays completed later heights.
///
/// After yielding the first error, the stream discards buffered later results
/// and terminates.
pub fn fetch_l1_block_range<'a, R>(
    fetcher: &'a R,
    start_height: L1Height,
    end_height: L1Height,
    policy: &'a FetchPolicy,
) -> Result<impl Stream<Item = Result<L1BlockData, FetchBlockError>> + 'a, FetchRangeError>
where
    R: L1BlockFetcher,
{
    if start_height > end_height {
        return Err(FetchRangeError::Inverted {
            start_height,
            end_height,
        });
    }

    let blocks = stream::iter(start_height..=end_height)
        .map(move |height| fetch_block_at(fetcher, height, policy.retry()))
        .buffered(policy.max_concurrent_blocks().get());

    Ok(stream::try_unfold(blocks, |mut blocks| async move {
        match blocks.next().await {
            Some(Ok(block)) => Ok(Some((block, blocks))),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    }))
}

fn is_retryable(error: &ClientError) -> bool {
    error.is_retriable() || matches!(error, ClientError::Server(BITCOIND_RPC_WARMUP, _))
}

async fn fetch_block_at<R>(
    fetcher: &R,
    height: L1Height,
    retry_policy: &FetchRetryPolicy,
) -> Result<L1BlockData, FetchBlockError>
where
    R: L1BlockFetcher,
{
    let mut retries = 0;
    let mut delay_ms = retry_policy.backoff().base_delay_ms();

    loop {
        match fetcher.get_block_at(height).await {
            Ok(block) => return Ok(L1BlockData::new(height, block)),
            Err(source) if !is_retryable(&source) => {
                return Err(FetchBlockError::Client { height, source });
            }
            Err(source) if retries >= retry_policy.max_retries() => {
                return Err(FetchBlockError::RetriesExhausted {
                    height,
                    max_retries: retry_policy.max_retries(),
                    source,
                });
            }
            Err(source) => {
                warn!(
                    height,
                    attempt = u32::from(retries) + 1,
                    max_attempts = u32::from(retry_policy.max_retries()) + 1,
                    retry_delay_ms = delay_ms,
                    err = %source,
                    "L1 block fetch failed; retrying"
                );
                retries += 1;
                sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = retry_policy.backoff().next_delay_ms(delay_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        future::pending,
        iter::repeat_with,
        sync::Mutex,
    };

    use bitcoin::{
        block::{Header, Version},
        hashes::Hash,
        Block, BlockHash, TxMerkleNode,
    };
    use bitcoind_async_client::error::ClientError;
    use futures::{poll, TryStreamExt};
    use strata_btc_types::BlockHashExt;
    use tokio::sync::oneshot;

    use super::*;

    enum MockBlockResponse {
        Ready(ClientResult<Block>),
        Controlled(oneshot::Receiver<ClientResult<Block>>),
        Pending,
    }

    #[derive(Default)]
    struct MockBlockFetcher {
        responses_by_height: Mutex<HashMap<L1Height, VecDeque<MockBlockResponse>>>,
        requested_heights: Mutex<Vec<L1Height>>,
    }

    impl MockBlockFetcher {
        fn with_block_responses(
            self,
            height: L1Height,
            responses: Vec<ClientResult<Block>>,
        ) -> Self {
            self.responses_by_height
                .lock()
                .expect("block responses lock")
                .insert(
                    height,
                    responses
                        .into_iter()
                        .map(MockBlockResponse::Ready)
                        .collect(),
                );
            self
        }

        fn with_controlled_block_responses(
            self,
            heights: impl IntoIterator<Item = L1Height>,
        ) -> (
            Self,
            HashMap<L1Height, oneshot::Sender<ClientResult<Block>>>,
        ) {
            let mut senders_by_height = HashMap::new();
            {
                let mut responses_by_height = self
                    .responses_by_height
                    .lock()
                    .expect("block responses lock");
                for height in heights {
                    let (sender, receiver) = oneshot::channel();
                    responses_by_height
                        .entry(height)
                        .or_default()
                        .push_back(MockBlockResponse::Controlled(receiver));
                    senders_by_height.insert(height, sender);
                }
            }

            (self, senders_by_height)
        }

        fn with_pending_block_responses(self, heights: impl IntoIterator<Item = L1Height>) -> Self {
            {
                let mut responses_by_height = self
                    .responses_by_height
                    .lock()
                    .expect("block responses lock");
                for height in heights {
                    responses_by_height.insert(height, [MockBlockResponse::Pending].into());
                }
            }
            self
        }

        fn requested_heights(&self) -> Vec<L1Height> {
            self.requested_heights
                .lock()
                .expect("block calls lock")
                .clone()
        }
    }

    impl L1BlockFetcher for MockBlockFetcher {
        async fn get_block_at(&self, height: L1Height) -> ClientResult<Block> {
            self.requested_heights
                .lock()
                .expect("block calls lock")
                .push(height);
            let response = self
                .responses_by_height
                .lock()
                .expect("block responses lock")
                .get_mut(&height)
                .and_then(VecDeque::pop_front)
                .expect("block response configured for requested height");

            match response {
                MockBlockResponse::Ready(response) => response,
                MockBlockResponse::Controlled(receiver) => {
                    receiver.await.expect("block response sender retained")
                }
                MockBlockResponse::Pending => pending().await,
            }
        }
    }

    fn make_fetch_policy() -> FetchPolicy {
        FetchPolicy::new(
            FetchRetryPolicy::new(5, ExponentialBackoff::new(0, 150, 100)),
            NonZeroUsize::new(5).expect("5 is always nonzero"),
        )
    }

    fn make_block_hash(seed: u8) -> BlockHash {
        BlockHash::from_byte_array([seed; 32])
    }

    fn build_block_with_prev_hash(hash: BlockHash) -> Block {
        Block {
            header: Header {
                version: Version::from_consensus(1),
                prev_blockhash: hash,
                merkle_root: TxMerkleNode::all_zeros(),
                time: 0,
                bits: Default::default(),
                nonce: 0,
            },
            txdata: Vec::new(),
        }
    }

    #[test]
    fn test_inverted_range_rejected() {
        let fetcher = MockBlockFetcher::default();
        let fetch_policy = make_fetch_policy();
        let err = match fetch_l1_block_range(&fetcher, 2, 1, &fetch_policy) {
            Ok(_) => panic!("range must reject"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            FetchRangeError::Inverted {
                start_height: 2,
                end_height: 1,
            }
        ));
    }

    #[tokio::test]
    async fn test_blocks_fetched_in_height_order() {
        let heights = [10, 11, 12];
        let mut fetcher = MockBlockFetcher::default();
        let expected = heights
            .iter()
            .map(|height| {
                let block = build_block_with_prev_hash(make_block_hash(*height as u8));
                (*height, block.block_hash().to_l1_block_id(), block)
            })
            .collect::<Vec<_>>();
        for (height, _, block) in &expected {
            fetcher = fetcher.with_block_responses(*height, vec![Ok(block.clone())]);
        }

        let fetch_policy = make_fetch_policy();
        let blocks = fetch_l1_block_range(&fetcher, 10, 12, &fetch_policy)
            .expect("stream builds")
            .try_collect::<Vec<_>>()
            .await
            .expect("fetch succeeds");

        assert_eq!(fetcher.requested_heights(), heights);
        for (block, (height, expected_block_id, _)) in blocks.iter().zip(expected.iter()) {
            assert_eq!(block.height(), *height);
            assert_eq!(block.block_id(), *expected_block_id);
        }
    }

    #[tokio::test]
    async fn test_concurrent_fetches_are_bounded() {
        let fetcher = MockBlockFetcher::default().with_pending_block_responses(10..=20);
        let max_concurrent_blocks = NonZeroUsize::new(3).expect("3 is always nonzero");
        let fetch_policy = FetchPolicy::new(
            FetchRetryPolicy::new(5, ExponentialBackoff::new(0, 150, 100)),
            max_concurrent_blocks,
        );
        let blocks = fetch_l1_block_range(&fetcher, 10, 20, &fetch_policy).expect("stream builds");
        futures::pin_mut!(blocks);

        assert!(poll!(blocks.next()).is_pending());
        assert_eq!(fetcher.requested_heights(), vec![10, 11, 12]);
    }

    #[tokio::test]
    async fn test_concurrent_fetch_preserves_height_order() {
        let heights = [10, 11, 12];
        let (fetcher, mut senders_by_height) =
            MockBlockFetcher::default().with_controlled_block_responses(heights);
        let fetch_policy = make_fetch_policy();
        let blocks = fetch_l1_block_range(&fetcher, 10, 12, &fetch_policy).expect("stream builds");
        futures::pin_mut!(blocks);

        assert!(poll!(blocks.next()).is_pending());
        for height in [12, 11] {
            let block = build_block_with_prev_hash(make_block_hash(height as u8));
            senders_by_height
                .remove(&height)
                .expect("block response sender exists")
                .send(Ok(block))
                .expect("block fetch remains active");
        }
        assert!(poll!(blocks.next()).is_pending());

        let first_block = build_block_with_prev_hash(make_block_hash(10));
        senders_by_height
            .remove(&10)
            .expect("block response sender exists")
            .send(Ok(first_block))
            .expect("block fetch remains active");

        let blocks = blocks
            .try_collect::<Vec<_>>()
            .await
            .expect("fetch succeeds");
        assert_eq!(
            blocks.iter().map(L1BlockData::height).collect::<Vec<_>>(),
            heights
        );
    }

    #[tokio::test]
    async fn test_concurrent_fetch_preserves_error_order() {
        let heights = [10, 11, 12];
        let fetcher = MockBlockFetcher::default()
            .with_block_responses(10, vec![Err(ClientError::Connection("retry".into()))]);
        let (fetcher, mut senders_by_height) = fetcher.with_controlled_block_responses(heights);
        let fetch_policy = FetchPolicy::new(
            FetchRetryPolicy::new(1, ExponentialBackoff::new(0, 150, 100)),
            NonZeroUsize::new(3).expect("3 is always nonzero"),
        );
        let blocks = fetch_l1_block_range(&fetcher, 10, 12, &fetch_policy).expect("stream builds");
        futures::pin_mut!(blocks);

        assert!(poll!(blocks.next()).is_pending());
        for height in [11, 12] {
            let block = build_block_with_prev_hash(make_block_hash(height as u8));
            senders_by_height
                .remove(&height)
                .expect("block response sender exists")
                .send(Ok(block))
                .expect("block fetch remains active");
        }
        assert!(poll!(blocks.next()).is_pending());

        senders_by_height
            .remove(&10)
            .expect("block response sender exists")
            .send(Err(ClientError::Connection("down".into())))
            .expect("block fetch remains active");

        let error = blocks
            .next()
            .await
            .expect("stream contains first height")
            .expect_err("first height must fail");
        assert!(matches!(
            error,
            FetchBlockError::RetriesExhausted {
                height: 10,
                max_retries: 1,
                source: ClientError::Connection(_),
            }
        ));
        assert!(blocks.next().await.is_none());
    }

    #[tokio::test]
    async fn test_max_height_fetched() {
        let height = L1Height::MAX;
        let block = build_block_with_prev_hash(make_block_hash(1));
        let fetcher = MockBlockFetcher::default().with_block_responses(height, vec![Ok(block)]);
        let fetch_policy = make_fetch_policy();

        let blocks = fetch_l1_block_range(&fetcher, height, height, &fetch_policy)
            .expect("stream builds")
            .try_collect::<Vec<_>>()
            .await
            .expect("fetch succeeds");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].height(), height);
    }

    #[tokio::test]
    async fn test_warmup_error_retried() {
        let block = build_block_with_prev_hash(make_block_hash(8));
        let fetcher = MockBlockFetcher::default().with_block_responses(
            100,
            vec![
                Err(ClientError::Server(
                    BITCOIND_RPC_WARMUP,
                    "Loading block index".into(),
                )),
                Ok(block),
            ],
        );

        let fetch_policy = make_fetch_policy();
        fetch_l1_block_range(&fetcher, 100, 100, &fetch_policy)
            .expect("stream builds")
            .try_collect::<Vec<_>>()
            .await
            .expect("retry succeeds");

        assert_eq!(fetcher.requested_heights(), vec![100, 100]);
    }

    #[tokio::test]
    async fn test_terminal_client_error_not_retried() {
        let fetcher = MockBlockFetcher::default().with_block_responses(
            100,
            vec![Err(ClientError::ReqBuilder("invalid request".into()))],
        );

        let fetch_policy = make_fetch_policy();
        let err = fetch_l1_block_range(&fetcher, 100, 100, &fetch_policy)
            .expect("stream builds")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("terminal client error must fail");

        assert!(matches!(
            err,
            FetchBlockError::Client {
                height: 100,
                source: ClientError::ReqBuilder(_),
            }
        ));
        assert_eq!(fetcher.requested_heights(), vec![100]);
    }

    #[tokio::test]
    async fn test_retry_exhaustion() {
        let fetch_policy = make_fetch_policy();
        let max_retries = fetch_policy.retry().max_retries();
        let attempt_count = usize::from(max_retries) + 1;
        let fetcher = MockBlockFetcher::default().with_block_responses(
            100,
            repeat_with(|| Err(ClientError::Connection("down".into())))
                .take(attempt_count)
                .collect(),
        );

        let err = fetch_l1_block_range(&fetcher, 100, 100, &fetch_policy)
            .expect("stream builds")
            .try_collect::<Vec<_>>()
            .await
            .expect_err("retry exhaustion must fail");

        assert!(matches!(
            err,
            FetchBlockError::RetriesExhausted {
                height: 100,
                max_retries: actual_max_retries,
                source: ClientError::Connection(_),
            } if actual_max_retries == max_retries
        ));
        assert_eq!(fetcher.requested_heights(), vec![100; attempt_count]);
    }
}
