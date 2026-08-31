use colored::Colorize;
pub mod backend;
pub mod persist;

use std::{
    fmt::Debug,
    io::{self},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::Arc,
};

use backend::{
    GetFeeRateError, InvalidFee, ScanError, SignetBackend, SyncError, UpdateError, WalletUpdate,
};
use bdk_esplora::esplora_client::{self, AsyncClient};
use bdk_wallet::{
    bitcoin::{constants::genesis_block, BlockHash, FeeRate, Network},
    rusqlite::{self, Connection},
    PersistedWallet, Wallet,
};
use persist::Persister;
use terrors::OneOf;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::seed::Seed;

const MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB: u64 = 1_000;

pub fn log_fee_rate(fr: &FeeRate) {
    println!(
        "Using {} as feerate",
        format!("{} sat/vb", fr.to_sat_per_vb_ceil()).green(),
    )
}

pub async fn get_fee_rate(
    user_provided_sats_per_vb: Option<u64>,
    signet_backend: &dyn SignetBackend,
) -> Result<FeeRate, OneOf<(InvalidFee, GetFeeRateError)>> {
    let fee_rate = match user_provided_sats_per_vb {
        Some(fr) => FeeRate::from_sat_per_vb(fr).ok_or(OneOf::new(InvalidFee))?,
        None => {
            let fee_rate = signet_backend
                .get_fee_rate(1)
                .await?
                .unwrap_or(FeeRate::BROADCAST_MIN);
            if fee_rate.to_sat_per_vb_ceil() > MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB {
                return Err(OneOf::new(GetFeeRateError::from_err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "automatic fee estimate exceeds {MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB} sat/vB; pass --fee-rate to override"
                    ),
                ))));
            }
            fee_rate
        }
    };

    Ok(fee_rate.max(FeeRate::BROADCAST_MIN))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use bdk_wallet::{
        bitcoin::{constants::genesis_block, BlockHash, FeeRate, Network, Transaction},
        chain::{
            spk_client::{FullScanRequestBuilder, SyncRequestBuilder},
            CheckPoint,
        },
        KeychainKind,
    };
    use terrors::OneOf;

    use super::{
        backend::{
            BroadcastTxError, GetFeeRateError, GetGenesisHashError, InvalidFee, ScanError,
            UpdateSender,
        },
        get_fee_rate, SignetBackend, SyncError, MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB,
    };

    #[derive(Debug)]
    struct TestSignetBackend {
        fee_rate: Option<FeeRate>,
    }

    #[async_trait]
    impl SignetBackend for TestSignetBackend {
        async fn get_genesis_hash(&self) -> Result<BlockHash, GetGenesisHashError> {
            Ok(genesis_block(Network::Signet).block_hash())
        }

        async fn sync_wallet(
            &self,
            _req: SyncRequestBuilder<(KeychainKind, u32)>,
            _last_cp: CheckPoint,
            _send_update: UpdateSender,
        ) -> Result<(), SyncError> {
            unimplemented!("not needed for fee rate tests")
        }

        async fn scan_wallet(
            &self,
            _req: FullScanRequestBuilder<KeychainKind>,
            _last_cp: CheckPoint,
            _send_update: UpdateSender,
        ) -> Result<(), ScanError> {
            unimplemented!("not needed for fee rate tests")
        }

        async fn broadcast_tx(&self, _tx: &Transaction) -> Result<(), BroadcastTxError> {
            unimplemented!("not needed for fee rate tests")
        }

        async fn get_fee_rate(
            &self,
            _target: u16,
        ) -> Result<Option<FeeRate>, OneOf<(InvalidFee, GetFeeRateError)>> {
            Ok(self.fee_rate)
        }
    }

    #[tokio::test]
    async fn test_get_fee_rate_clamps_backend_zero_to_broadcast_minimum() {
        let backend = TestSignetBackend {
            fee_rate: Some(FeeRate::ZERO),
        };

        let fee_rate = get_fee_rate(None, &backend).await.unwrap();

        assert_eq!(fee_rate, FeeRate::BROADCAST_MIN);
    }

    #[tokio::test]
    async fn test_get_fee_rate_uses_broadcast_minimum_when_backend_has_no_estimate() {
        let backend = TestSignetBackend { fee_rate: None };

        let fee_rate = get_fee_rate(None, &backend).await.unwrap();

        assert_eq!(fee_rate, FeeRate::BROADCAST_MIN);
    }

    #[tokio::test]
    async fn test_get_fee_rate_clamps_user_zero_to_broadcast_minimum() {
        let backend = TestSignetBackend { fee_rate: None };

        let fee_rate = get_fee_rate(Some(0), &backend).await.unwrap();

        assert_eq!(fee_rate, FeeRate::BROADCAST_MIN);
    }

    #[tokio::test]
    async fn automatic_fee_rate_has_a_safety_limit() {
        let backend = TestSignetBackend {
            fee_rate: FeeRate::from_sat_per_vb(MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB + 1),
        };

        assert!(get_fee_rate(None, &backend).await.is_err());
        assert!(
            get_fee_rate(Some(MAX_AUTOMATIC_FEE_RATE_SATS_PER_VB + 1), &backend)
                .await
                .is_ok()
        );
    }
}

#[derive(Clone, Debug)]
pub struct EsploraClient(AsyncClient);

impl DerefMut for EsploraClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for EsploraClient {
    type Target = AsyncClient;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EsploraClient {
    pub fn new(esplora_url: &str) -> Result<Self, esplora_client::Error> {
        Ok(Self(
            esplora_client::Builder::new(esplora_url).build_async()?,
        ))
    }
}

#[derive(Debug)]
/// A wrapper around BDK's wallet with some custom logic
pub struct SignetWallet {
    wallet: PersistedWallet<Persister>,
    sync_backend: Arc<dyn SignetBackend>,
    network: Network,
}

impl SignetWallet {
    pub(crate) fn db_path(wallet: &str, data_dir: &Path, network: Network) -> PathBuf {
        let wallet = match network {
            Network::Bitcoin => format!("{wallet}-bitcoin"),
            _ => wallet.to_string(),
        };
        data_dir.join(wallet).with_extension("sqlite")
    }

    pub fn persister(data_dir: &Path, network: Network) -> Result<Connection, rusqlite::Error> {
        Connection::open(Self::db_path("default", data_dir, network))
    }

    pub fn new(
        seed: &Seed,
        network: Network,
        sync_backend: Arc<dyn SignetBackend>,
    ) -> io::Result<Self> {
        let (load, create) = seed.bitcoin_wallet(network).split();
        Ok(Self {
            wallet: load
                .check_network(network)
                .load_wallet(&mut Persister)
                .expect("should be able to load wallet")
                .unwrap_or_else(|| {
                    create
                        .network(network)
                        .create_wallet(&mut Persister)
                        .expect("wallet creation to succeed")
                }),
            sync_backend,
            network,
        })
    }

    pub async fn sync(&mut self) -> Result<(), OneOf<(UpdateError, SyncError, rusqlite::Error)>> {
        let genesis = self
            .sync_backend
            .get_genesis_hash()
            .await
            .map_err(|error| OneOf::new(SyncError::from_err(error)))?;
        validate_backend_network(self.network, genesis)
            .map_err(|error| OneOf::new(SyncError::from_err(error)))?;
        sync_wallet(&mut self.wallet, self.sync_backend.clone()).await?;
        self.persist().map_err(OneOf::new)?;
        Ok(())
    }

    pub async fn scan(&mut self) -> Result<(), OneOf<(UpdateError, ScanError, rusqlite::Error)>> {
        let genesis = self
            .sync_backend
            .get_genesis_hash()
            .await
            .map_err(|error| OneOf::new(ScanError::from_err(error)))?;
        validate_backend_network(self.network, genesis)
            .map_err(|error| OneOf::new(ScanError::from_err(error)))?;
        scan_wallet(&mut self.wallet, self.sync_backend.clone()).await?;
        self.persist().map_err(OneOf::new)?;
        Ok(())
    }

    pub fn persist(&mut self) -> Result<bool, rusqlite::Error> {
        self.wallet.persist(&mut Persister)
    }
}

fn validate_backend_network(network: Network, actual: BlockHash) -> io::Result<()> {
    let expected = genesis_block(network).block_hash();
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Bitcoin backend network mismatch: expected {expected}, received {actual}"),
        ));
    }
    Ok(())
}

pub async fn scan_wallet(
    wallet: &mut Wallet,
    sync_backend: Arc<dyn SignetBackend>,
) -> Result<(), OneOf<(UpdateError, ScanError, rusqlite::Error)>> {
    let req = wallet.start_full_scan();
    let last_cp = wallet.latest_checkpoint();
    let (tx, rx) = unbounded_channel();

    let handle = tokio::spawn(async move { sync_backend.scan_wallet(req, last_cp, tx).await });

    apply_update_stream(wallet, rx).await.map_err(OneOf::new)?;

    handle
        .await
        .expect("thread to be fine")
        .map_err(OneOf::new)?;

    Ok(())
}

pub async fn sync_wallet(
    wallet: &mut Wallet,
    sync_backend: Arc<dyn SignetBackend>,
) -> Result<(), OneOf<(UpdateError, SyncError, rusqlite::Error)>> {
    let req = wallet.start_sync_with_revealed_spks();
    let last_cp = wallet.latest_checkpoint();
    let (tx, rx) = unbounded_channel();

    let handle = tokio::spawn(async move { sync_backend.sync_wallet(req, last_cp, tx).await });

    apply_update_stream(wallet, rx).await.map_err(OneOf::new)?;

    handle
        .await
        .expect("thread to be fine")
        .map_err(OneOf::new)?;

    Ok(())
}

async fn apply_update_stream(
    wallet: &mut Wallet,
    mut rx: UnboundedReceiver<WalletUpdate>,
) -> Result<(), UpdateError> {
    while let Some(update) = rx.recv().await {
        match update {
            WalletUpdate::SpkSync(update) => {
                wallet.apply_update(update).map_err(UpdateError::from_err)?
            }
            WalletUpdate::SpkScan(update) => {
                wallet.apply_update(update).map_err(UpdateError::from_err)?
            }
            WalletUpdate::NewBlock(ev) => {
                let height = ev.block_height();
                let connected_to = ev.connected_to();
                wallet
                    .apply_block_connected_to(&ev.block, height, connected_to)
                    .map_err(UpdateError::from_err)?
            }
            WalletUpdate::MempoolTxs(txs) => wallet.apply_unconfirmed_txs(txs),
        }
    }

    Ok(())
}

impl Deref for SignetWallet {
    type Target = PersistedWallet<Persister>;

    fn deref(&self) -> &Self::Target {
        &self.wallet
    }
}

impl DerefMut for SignetWallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.wallet
    }
}
