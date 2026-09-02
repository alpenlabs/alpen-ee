//! Resolves the Bitcoin fee rate used as input to DA pricing.
//!
//! This private copy preserves the fee-policy behavior from the currently
//! pinned `strata-btcio`.
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
use tracing::warn;

const MEMPOOL_FEE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MEMPOOL_FEE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

static SHARED_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(MEMPOOL_FEE_REQUEST_TIMEOUT)
        .connect_timeout(MEMPOOL_FEE_CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|err| {
            warn!(%err, "falling back to an untimed HTTP client for mempool fee lookups");
            reqwest::Client::new()
        })
});

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
        let mut url = Url::parse(base_url)
            .with_context(|| format!("invalid mempool_base_url: {base_url}"))?;

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
            .with_context(|| format!("invalid path URL for base: {}", self.base_url))?;

        SHARED_HTTP_CLIENT
            .get(url)
            .send()
            .await
            .context("failed to call mempool recommended fees endpoint")?
            .error_for_status()
            .context("mempool recommended fees endpoint returned an error status")?
            .json::<MempoolRecommendedFees>()
            .await
            .context("failed to decode mempool recommended fees response")
    }

    async fn fetch_recommended_fees(&self) -> anyhow::Result<MempoolRecommendedFees> {
        match self.fetch_fee_estimates("api/v1/fees/precise").await {
            Ok(fees) => Ok(fees),
            Err(err) => {
                warn!(
                    %err,
                    "mempool precise fee lookup failed, falling back to recommended endpoint"
                );
                self.fetch_fee_estimates("api/v1/fees/recommended").await
            }
        }
    }
}

/// Resolves a Bitcoin fee rate with the configured writer fee policy.
pub(super) async fn resolve_fee_rate<R>(
    client: &R,
    config: &L1FeePolicyConfig,
) -> anyhow::Result<FeeRate>
where
    R: Reader,
{
    match config.fee_policy() {
        FeePolicy::BitcoinD { conf_target } => client
            .estimate_smart_fee(*conf_target)
            .await
            .context("failed to estimate smart fee")
            .and_then(|estimate| {
                estimate.fee_rate.ok_or_else(|| {
                    anyhow::anyhow!("smart fee estimate unavailable: {:?}", estimate.errors)
                })
            }),
        FeePolicy::MempoolExplorer {
            policy,
            mempool_base_url,
            fallback_conf_target,
        } => {
            resolve_mempool_fee_rate(client, mempool_base_url, *fallback_conf_target, *policy).await
        }
        FeePolicy::Fixed { fee_rate } => Ok(*fee_rate),
    }
}

async fn resolve_mempool_fee_rate<R>(
    client: &R,
    base_url: &str,
    fallback_conf_target: u16,
    mempool_fee_policy: MempoolExplorerFeePolicy,
) -> anyhow::Result<FeeRate>
where
    R: Reader,
{
    let explorer = MempoolExplorerClient::new(base_url)?;

    match explorer.fetch_recommended_fees().await {
        Ok(fees) => fees.select(mempool_fee_policy),
        Err(err) => {
            warn!(
                %base_url,
                %err,
                fallback_conf_target,
                "mempool fee lookup failed, falling back to bitcoind's estimatesmartfee"
            );
            client
                .estimate_smart_fee(fallback_conf_target)
                .await
                .context("failed to estimate smart fee after mempool fallback")
                .and_then(|estimate| {
                    estimate.fee_rate.ok_or_else(|| {
                        anyhow::anyhow!("smart fee estimate unavailable: {:?}", estimate.errors)
                    })
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bitcoind_async_client::{Auth, Client as BtcClient};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
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

    #[tokio::test]
    async fn fixed_policy_returns_configured_bitcoin_fee_rate() {
        let client = disconnected_bitcoin_client();
        let expected = FeeRate::from_sat_per_kwu(125);
        let config = policy_config(FeePolicy::Fixed { fee_rate: expected });

        assert_eq!(resolve_fee_rate(&client, &config).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn bitcoind_policy_uses_configured_confirmation_target() {
        let response =
            r#"{"result":{"feerate":0.00003,"errors":null,"blocks":6},"error":null,"id":0}"#;
        let (server, requests) = spawn_response_server(vec![("200 OK", response)]).await;
        let client = bitcoin_client(server);
        let config = policy_config(FeePolicy::BitcoinD { conf_target: 6 });

        assert_eq!(
            resolve_fee_rate(&client, &config).await.unwrap(),
            FeeRate::from_sat_per_vb_u32(3)
        );
        assert!(requests.lock().unwrap()[0].contains("\"params\":[6]"));
    }

    #[tokio::test]
    async fn mempool_policy_uses_selected_recommendation() {
        let body = r#"{"fastestFee":7,"halfHourFee":6,"hourFee":5,"economyFee":4,"minimumFee":3}"#;
        let (server, _) = spawn_response_server(vec![("200 OK", body)]).await;
        let client = disconnected_bitcoin_client();
        let config = mempool_config(MempoolExplorerFeePolicy::Economy, server);

        assert_eq!(
            resolve_fee_rate(&client, &config).await.unwrap(),
            FeeRate::from_sat_per_vb_u32(4)
        );
    }

    #[tokio::test]
    async fn mempool_failure_falls_back_to_bitcoind() {
        let (mempool_server, _) = spawn_response_server(vec![
            ("500 Internal Server Error", "{}"),
            ("200 OK", "not-json"),
        ])
        .await;
        let response =
            r#"{"result":{"feerate":0.00002,"errors":null,"blocks":3},"error":null,"id":0}"#;
        let (bitcoin_server, requests) = spawn_response_server(vec![("200 OK", response)]).await;
        let client = bitcoin_client(bitcoin_server);
        let config = mempool_config(MempoolExplorerFeePolicy::Fastest, mempool_server);

        assert_eq!(
            resolve_fee_rate(&client, &config).await.unwrap(),
            FeeRate::from_sat_per_vb_u32(2)
        );
        assert!(requests.lock().unwrap()[0].contains("\"params\":[3]"));
    }

    #[tokio::test]
    async fn malformed_mempool_url_is_rejected() {
        let client = disconnected_bitcoin_client();
        let config = mempool_config(MempoolExplorerFeePolicy::Fastest, "not a url".to_string());

        let error = resolve_fee_rate(&client, &config).await.unwrap_err();

        assert!(error.to_string().contains("invalid mempool_base_url"));
    }
}
