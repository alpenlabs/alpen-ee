//! Resolves the Bitcoin fee rate used as input to DA pricing.
//!
//! This private copy preserves the fee-selection and fallback behavior from
//! the currently pinned `strata-btcio`. The DA service supplies its own
//! timeout budgets.
//!
//! NOTE: This needs to be removed once that crate exposes the same
//! operation as a public API.

use std::{sync::LazyLock, time::Duration};

use anyhow::Context;
use bitcoind_async_client::{corepc_types::bitcoin::FeeRate, traits::Reader};
use reqwest::Url;
use serde::Deserialize;
use strata_config::btcio::{
    fee_rate_from_sat_per_vb, FeePolicy, L1FeePolicyConfig, MempoolExplorerFeePolicy,
};
use tokio::time::timeout;
use tracing::warn;

static SHARED_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Bounds the external phases of one writer-backed fee-rate resolution.
#[derive(Clone, Copy, Debug)]
pub(super) struct FeeRateResolutionTimeouts {
    explorer: Duration,
    bitcoind: Duration,
}

impl FeeRateResolutionTimeouts {
    /// Creates independent explorer and Bitcoin Core timeout budgets.
    pub(super) const fn new(explorer: Duration, bitcoind: Duration) -> Self {
        Self { explorer, bitcoind }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct MempoolRecommendedFees {
    #[serde(rename = "fastestFee")]
    fastest_fee: f64,
    #[serde(rename = "halfHourFee")]
    half_hour_fee: f64,
    #[serde(rename = "hourFee")]
    hour_fee: f64,
    #[serde(rename = "economyFee")]
    economy_fee: f64,
    #[serde(rename = "minimumFee")]
    minimum_fee: f64,
}

impl MempoolRecommendedFees {
    fn select(self, policy: MempoolExplorerFeePolicy) -> anyhow::Result<FeeRate> {
        let fee_rate_sat_per_vb = match policy {
            MempoolExplorerFeePolicy::Fastest => self.fastest_fee,
            MempoolExplorerFeePolicy::HalfHour => self.half_hour_fee,
            MempoolExplorerFeePolicy::Hour => self.hour_fee,
            MempoolExplorerFeePolicy::Economy => self.economy_fee,
            MempoolExplorerFeePolicy::Minimum => self.minimum_fee,
        };
        fee_rate_from_sat_per_vb(fee_rate_sat_per_vb).map_err(anyhow::Error::msg)
    }
}

struct MempoolExplorerClient {
    base_url: Url,
}

impl MempoolExplorerClient {
    fn new(base_url: &str) -> anyhow::Result<Self> {
        let mut url = Url::parse(base_url).context("invalid mempool_base_url")?;

        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }

        Ok(Self { base_url: url })
    }

    async fn fetch_fee_estimates(&self, path: &str) -> anyhow::Result<MempoolRecommendedFees> {
        let url = self
            .base_url
            .join(path)
            .context("invalid mempool fee endpoint path")?;

        SHARED_HTTP_CLIENT
            .get(url)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("failed to call mempool recommended fees endpoint"))?
            .error_for_status()
            .map_err(|_| {
                anyhow::anyhow!("mempool recommended fees endpoint returned an error status")
            })?
            .json::<MempoolRecommendedFees>()
            .await
            .map_err(|_| anyhow::anyhow!("failed to decode mempool recommended fees response"))
    }

    async fn fetch_fee_rate(
        &self,
        path: &str,
        policy: MempoolExplorerFeePolicy,
    ) -> anyhow::Result<FeeRate> {
        self.fetch_fee_estimates(path).await?.select(policy)
    }

    async fn fetch_recommended_fee_rate(
        &self,
        policy: MempoolExplorerFeePolicy,
    ) -> anyhow::Result<FeeRate> {
        match self.fetch_fee_rate("api/v1/fees/precise", policy).await {
            Ok(fee_rate) => Ok(fee_rate),
            Err(err) => {
                warn!(
                    %err,
                    "mempool precise fee lookup failed, falling back to recommended endpoint"
                );
                self.fetch_fee_rate("api/v1/fees/recommended", policy).await
            }
        }
    }
}

/// Resolves a Bitcoin fee rate with the configured writer fee policy.
pub(super) async fn resolve_fee_rate<R>(
    client: &R,
    config: &L1FeePolicyConfig,
    timeouts: FeeRateResolutionTimeouts,
) -> anyhow::Result<FeeRate>
where
    R: Reader,
{
    match config.fee_policy() {
        FeePolicy::BitcoinD { conf_target } => {
            resolve_smart_fee_rate(client, *conf_target, timeouts.bitcoind).await
        }
        FeePolicy::MempoolExplorer {
            policy,
            mempool_base_url,
            fallback_conf_target,
        } => {
            resolve_mempool_fee_rate(
                client,
                mempool_base_url,
                *fallback_conf_target,
                *policy,
                timeouts,
            )
            .await
        }
        FeePolicy::Fixed { fee_rate } => Ok(*fee_rate),
    }
}

async fn resolve_mempool_fee_rate<R>(
    client: &R,
    base_url: &str,
    fallback_conf_target: u16,
    mempool_fee_policy: MempoolExplorerFeePolicy,
    timeouts: FeeRateResolutionTimeouts,
) -> anyhow::Result<FeeRate>
where
    R: Reader,
{
    let explorer = MempoolExplorerClient::new(base_url)?;

    let explorer_error = match timeout(
        timeouts.explorer,
        explorer.fetch_recommended_fee_rate(mempool_fee_policy),
    )
    .await
    {
        Ok(Ok(fee_rate)) => return Ok(fee_rate),
        Ok(Err(err)) => err,
        Err(err) => anyhow::Error::new(err).context(format!(
            "mempool fee lookup timed out after {:?}",
            timeouts.explorer
        )),
    };

    warn!(
        %explorer_error,
        fallback_conf_target,
        "mempool fee lookup failed, falling back to bitcoind's estimatesmartfee"
    );
    resolve_smart_fee_rate(client, fallback_conf_target, timeouts.bitcoind)
        .await
        .context("failed to estimate smart fee after mempool fallback")
}

async fn resolve_smart_fee_rate<R>(
    client: &R,
    conf_target: u16,
    request_timeout: Duration,
) -> anyhow::Result<FeeRate>
where
    R: Reader,
{
    let estimate = timeout(request_timeout, client.estimate_smart_fee(conf_target))
        .await
        .with_context(|| {
            format!("Bitcoin Core fee-rate lookup timed out after {request_timeout:?}")
        })?
        .context("failed to estimate smart fee")?;

    estimate
        .fee_rate
        .ok_or_else(|| anyhow::anyhow!("smart fee estimate unavailable: {:?}", estimate.errors))
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{Arc, Mutex},
    };

    use bitcoind_async_client::{Auth, Client as BtcClient};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::error::Elapsed,
    };

    use super::*;

    fn policy_config(fee_policy: FeePolicy) -> L1FeePolicyConfig {
        L1FeePolicyConfig::new(fee_policy)
    }

    fn mempool_config(
        policy: MempoolExplorerFeePolicy,
        mempool_base_url: String,
    ) -> L1FeePolicyConfig {
        policy_config(FeePolicy::MempoolExplorer {
            policy,
            mempool_base_url,
            fallback_conf_target: 3,
        })
    }

    fn bitcoin_client(url: String) -> BtcClient {
        BtcClient::new(
            url,
            Auth::UserPass("test-user".to_string(), "test-password".to_string()),
            Some(1),
            Some(0),
            Some(1),
        )
        .expect("test Bitcoin client should be constructed")
    }

    fn disconnected_bitcoin_client() -> BtcClient {
        bitcoin_client("http://127.0.0.1:1".to_string())
    }

    fn resolution_timeouts() -> FeeRateResolutionTimeouts {
        FeeRateResolutionTimeouts::new(Duration::from_secs(10), Duration::from_secs(10))
    }

    async fn spawn_response_server(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should have an address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = requests.clone();

        tokio::spawn(async move {
            for (status_line, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("accept should succeed");
                let mut request = [0_u8; 1024];
                let bytes_read = stream
                    .read(&mut request)
                    .await
                    .expect("request read should succeed");
                recorded_requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..bytes_read]).into_owned());
                let response = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response write should succeed");
            }
        });

        (format!("http://{addr}"), requests)
    }

    async fn spawn_stalled_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should have an address");

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0_u8; 1024];
            let _bytes_read = stream
                .read(&mut request)
                .await
                .expect("request read should succeed");
            pending::<()>().await;
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fixed_policy_returns_configured_bitcoin_fee_rate() {
        let client = disconnected_bitcoin_client();
        let expected = FeeRate::from_sat_per_kwu(125);
        let config = policy_config(FeePolicy::Fixed { fee_rate: expected });

        assert_eq!(
            resolve_fee_rate(&client, &config, resolution_timeouts())
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn bitcoind_policy_uses_configured_confirmation_target() {
        let response =
            r#"{"result":{"feerate":0.00003,"errors":null,"blocks":6},"error":null,"id":0}"#;
        let (server, requests) = spawn_response_server(vec![("200 OK", response)]).await;
        let client = bitcoin_client(server);
        let config = policy_config(FeePolicy::BitcoinD { conf_target: 6 });

        assert_eq!(
            resolve_fee_rate(&client, &config, resolution_timeouts())
                .await
                .unwrap(),
            FeeRate::from_sat_per_vb_u32(3)
        );
        assert!(requests.lock().unwrap()[0].contains("\"params\":[6]"));
    }

    #[tokio::test]
    async fn mempool_policy_uses_selected_recommendation() {
        let invalid_precise =
            r#"{"fastestFee":7,"halfHourFee":6,"hourFee":5,"economyFee":-1,"minimumFee":3}"#;
        let recommended =
            r#"{"fastestFee":7,"halfHourFee":6,"hourFee":5,"economyFee":4,"minimumFee":3}"#;
        let (server, requests) =
            spawn_response_server(vec![("200 OK", invalid_precise), ("200 OK", recommended)]).await;
        let client = disconnected_bitcoin_client();
        let config = mempool_config(MempoolExplorerFeePolicy::Economy, server);

        assert_eq!(
            resolve_fee_rate(&client, &config, resolution_timeouts())
                .await
                .unwrap(),
            FeeRate::from_sat_per_vb_u32(4)
        );
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("/api/v1/fees/precise"));
        assert!(requests[1].contains("/api/v1/fees/recommended"));
    }

    #[tokio::test]
    async fn mempool_failure_falls_back_to_bitcoind() {
        let invalid =
            r#"{"fastestFee":-1,"halfHourFee":6,"hourFee":5,"economyFee":4,"minimumFee":3}"#;
        let overflowing =
            r#"{"fastestFee":1e20,"halfHourFee":6,"hourFee":5,"economyFee":4,"minimumFee":3}"#;
        let (mempool_server, _) =
            spawn_response_server(vec![("200 OK", invalid), ("200 OK", overflowing)]).await;
        let response =
            r#"{"result":{"feerate":0.00002,"errors":null,"blocks":3},"error":null,"id":0}"#;
        let (bitcoin_server, requests) = spawn_response_server(vec![("200 OK", response)]).await;
        let client = bitcoin_client(bitcoin_server);
        let config = mempool_config(MempoolExplorerFeePolicy::Fastest, mempool_server);

        assert_eq!(
            resolve_fee_rate(&client, &config, resolution_timeouts())
                .await
                .unwrap(),
            FeeRate::from_sat_per_vb_u32(2)
        );
        assert!(requests.lock().unwrap()[0].contains("\"params\":[3]"));
    }

    #[tokio::test]
    async fn mempool_timeout_leaves_time_for_bitcoind_fallback() {
        let mempool_server = spawn_stalled_server().await;
        let response =
            r#"{"result":{"feerate":0.00002,"errors":null,"blocks":3},"error":null,"id":0}"#;
        let (bitcoin_server, requests) = spawn_response_server(vec![("200 OK", response)]).await;
        let client = bitcoin_client(bitcoin_server);
        let config = mempool_config(MempoolExplorerFeePolicy::Fastest, mempool_server);
        let timeouts =
            FeeRateResolutionTimeouts::new(Duration::from_millis(50), Duration::from_secs(1));

        assert_eq!(
            resolve_fee_rate(&client, &config, timeouts).await.unwrap(),
            FeeRate::from_sat_per_vb_u32(2)
        );
        assert!(requests.lock().unwrap()[0].contains("\"params\":[3]"));
    }

    #[tokio::test]
    async fn bitcoind_lookup_uses_its_own_timeout() {
        let bitcoin_server = spawn_stalled_server().await;
        let client = bitcoin_client(bitcoin_server);
        let config = policy_config(FeePolicy::BitcoinD { conf_target: 6 });
        let timeouts =
            FeeRateResolutionTimeouts::new(Duration::from_secs(1), Duration::from_millis(50));

        let error = resolve_fee_rate(&client, &config, timeouts)
            .await
            .unwrap_err();

        assert!(error.is::<Elapsed>());
        assert!(format!("{error:#}").contains("Bitcoin Core fee-rate lookup timed out"));
    }

    #[tokio::test]
    async fn malformed_mempool_url_is_rejected() {
        let client = disconnected_bitcoin_client();
        let config = mempool_config(MempoolExplorerFeePolicy::Fastest, "not a url".to_string());

        let error = resolve_fee_rate(&client, &config, resolution_timeouts())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("invalid mempool_base_url"));
    }
}
