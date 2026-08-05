//! Sequencer-only startup: boot-state init, the DA/btcio pipeline, and the
//! batch/chunk builder services that only run in sequencer mode.
//!
//! [`launch`] is the sole entry point once the reth node exists: it resolves
//! the writer config, block-builder config, boot state, and DA reveal
//! signing key itself, so callers only need to know a node exists and that
//! it is running as a sequencer. [`initial_preconf_head`] is the one other
//! entry point, needed earlier — before the node is built — to seed the p2p
//! preconf head watch with the sequencer's real exec-chain tip.

mod da_pipeline;
mod gas_data_provider;
mod header_summary;
mod payload_builder;
mod prover;
mod provers;
mod services;

use std::sync::Arc;

use alpen_ee_common::{require_latest_batch, BlockNumHash, SequencerOLClient};
use alpen_ee_database::EeNodeStorage;
use alpen_ee_exec_chain::{init_exec_chain_state_from_storage, ExecChainState};
use alpen_ee_sequencer::{
    block_builder_task, build_ol_chain_tracker, create_batch_builder, create_batch_lifecycle_task,
    create_update_submitter_task, init_batch_builder_state, init_lifecycle_state,
    init_ol_chain_tracker_state,
    sealing_policy::{
        block_count_policy::{BlockCountDataProvider, BlockCountPolicy, FixedBlockCountSealing},
        gas_limit_policy::MaxGasSealing,
        or_policy::OrSealing,
    },
    BatchBuilderEvent, BatchBuilderState, BatchLifecycleState, BlockBuilderConfig,
    OLChainTrackerState,
};
use alpen_reth_node::AlpenEngineTypes;
use bitcoind_async_client::corepc_types::bitcoin::{
    key::Keypair,
    secp256k1::{Secp256k1, SecretKey},
};
use eyre::Context;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_tasks::TaskExecutor;
use strata_config::btcio::{fee_rate_to_sat_per_vb, FeePolicy, WriterConfig};
use strata_primitives::buf::Buf32;
use tokio::sync::{mpsc, watch};
use tracing::{info, info_span, Instrument};

use self::{gas_data_provider::RethGasDataProvider, payload_builder::AlpenRethPayloadEngine};
use crate::{
    args::sequencer_privkey_from_env,
    config::{AlpenClientConfig, NodeMode},
    node::NodeBootstrap,
};

/// Startup state that only the EE sequencer needs: the OL chain tracker,
/// exec chain, batch builder, and batch lifecycle states loaded from
/// storage once the node is up.
struct SequencerBootState {
    ol_chain_tracker: OLChainTrackerState,
    exec_chain: ExecChainState,
    batch_builder: BatchBuilderState<BlockCountPolicy>,
    batch_lifecycle: BatchLifecycleState,
}

/// The sequencer's exec-chain tip, used to seed the p2p preconf head watch
/// before the reth node (and therefore the engine-control task) is built.
///
/// Only loads the exec-chain piece of boot state; [`launch`] loads the full
/// [`SequencerBootState`] again once the node is up. Both reads are cheap,
/// local, read-only sled reads with nothing else touching storage in
/// between, so re-reading is simpler than threading a boot-state value
/// across the generic parts of node startup.
pub(crate) async fn initial_preconf_head(storage: &EeNodeStorage) -> eyre::Result<BlockNumHash> {
    let exec_chain = init_exec_chain_state_from_storage(storage)
        .instrument(info_span!(
            "init_exec_chain_head_probe",
            component = "alpen"
        ))
        .await
        .context("exec chain state initialization should not fail")?;
    Ok(exec_chain.tip_blocknumhash())
}

fn log_writer_config(cfg: &WriterConfig) {
    match cfg.fee_policy() {
        FeePolicy::BitcoinD { conf_target } => {
            info!(target: "alpen-client",
            component = "alpen",
            policy = "bitcoind",
            conf_target, "btcio writer configured",);
        }
        FeePolicy::Fixed { fee_rate } => {
            info!(
                target: "alpen-client",
                component = "alpen",
                policy = "fixed",
                fee_rate_sat_vb = fee_rate_to_sat_per_vb(*fee_rate),
                "btcio writer configured",
            );
        }
        FeePolicy::MempoolExplorer {
            policy,
            mempool_base_url,
            fallback_conf_target,
        } => {
            info!(
                target: "alpen-client",
                component = "alpen",
                policy = "mempool",
                tier = ?policy,
                base_url = %mempool_base_url,
                fallback_conf_target,
                "btcio writer configured",
            );
        }
    }
}

pub(crate) fn sequencer_bitcoin_keypair(privkey: &Buf32) -> eyre::Result<Keypair> {
    let sk = SecretKey::from_slice(privkey.as_ref()).context("invalid sequencer private key")?;
    let secp = Secp256k1::signing_only();
    Ok(Keypair::from_secret_key(&secp, &sk))
}

/// Derives the sequencer's gossip pubkey from its private key, rather than
/// taking it as separate config — a sequencer can't be told a pubkey that
/// disagrees with its private key if there's no second value to disagree.
pub(crate) fn sequencer_gossip_pubkey(privkey: &Buf32) -> eyre::Result<Buf32> {
    let keypair = sequencer_bitcoin_keypair(privkey)?;
    let (x_only_pubkey, _parity) = keypair.x_only_public_key();
    Ok(Buf32(x_only_pubkey.serialize()))
}

/// Loads sequencer boot state: OL chain tracker, exec chain, batch builder,
/// and batch lifecycle.
async fn init_boot_state(
    storage: &EeNodeStorage,
    ol_client: &(impl SequencerOLClient + Send + Sync),
) -> eyre::Result<SequencerBootState> {
    let ol_chain_tracker = init_ol_chain_tracker_state(storage, ol_client)
        .instrument(info_span!("init_ol_chain_tracker", component = "alpen"))
        .await
        .context("ol chain tracker state initialization should not fail")?;
    let exec_chain = init_exec_chain_state_from_storage(storage)
        .instrument(info_span!("init_exec_chain", component = "alpen"))
        .await
        .context("exec chain state initialization should not fail")?;
    let batch_builder = init_batch_builder_state(storage)
        .instrument(info_span!("init_batch_builder", component = "alpen"))
        .await
        .context("batch builder state initialization should not fail")?;
    let batch_lifecycle = init_lifecycle_state(storage)
        .instrument(info_span!("init_lifecycle", component = "alpen"))
        .await
        .context("batch lifecycle state initialization should not fail")?;

    Ok(SequencerBootState {
        ol_chain_tracker,
        exec_chain,
        batch_builder,
        batch_lifecycle,
    })
}

/// The pieces of sequencer startup that exist only because the reth node has
/// launched, and so can't be reached through [`NodeBootstrap`] or
/// [`AlpenClientConfig`]: the node's handles, plus the preconf head channel
/// that is created alongside them.
///
/// `P` is the reth node's state/block/header provider type; kept generic
/// (rather than naming the concrete `FullNode<...>` type) since only three
/// call sites here need it.
pub(crate) struct RethNodeParts<P> {
    pub(crate) node_provider: P,
    pub(crate) task_executor: TaskExecutor,
    pub(crate) payload_builder_handle: PayloadBuilderHandle<AlpenEngineTypes>,
    pub(crate) beacon_engine_handle: ConsensusEngineHandle<AlpenEngineTypes>,
    /// Both ends are handed over rather than deriving the receiver from the
    /// sender here: `Sender::subscribe` marks the current value as already
    /// seen, so a head update published between channel creation and this
    /// point would be missed by the batch builder.
    pub(crate) preconf_tx: watch::Sender<BlockNumHash>,
    pub(crate) preconf_rx: watch::Receiver<BlockNumHash>,
}

/// Launches every service that only runs in sequencer mode: the exec chain /
/// OL chain tracker, the DA (btcio) pipeline, the EE chunk + acct provers,
/// and the batch/chunk builder services.
///
/// Takes the shared node bootstrap and the whole client config, and pulls
/// what it needs out of them itself. It also resolves the btcio writer
/// config, block-builder config, boot state, and the DA reveal signing key,
/// so callers only need to know that a node exists and that it is running as
/// a sequencer.
pub(crate) async fn launch<P>(
    common: &NodeBootstrap,
    alpen_config: &AlpenClientConfig,
    node_parts: RethNodeParts<P>,
) -> eyre::Result<()>
where
    P: StateProviderFactory
        + BlockReader<Block = reth_primitives::Block>
        + HeaderProvider<Header = reth_primitives::Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let RethNodeParts {
        node_provider,
        task_executor,
        payload_builder_handle,
        beacon_engine_handle,
        preconf_tx,
        preconf_rx,
    } = node_parts;

    // `node::run_node` only calls this in sequencer mode, so the other arm is
    // a caller bug rather than a config problem.
    let NodeMode::Sequencer(sequencer_mode) = &alpen_config.mode else {
        eyre::bail!("sequencer::launch called on a node that is not a sequencer");
    };
    let sequencer_config = &sequencer_mode.config;

    let NodeBootstrap {
        service_executor,
        dbs,
        storage,
        ol_client,
        params,
        ol_tracker,
        ..
    } = common;
    let consensus_watcher = ol_tracker.consensus_watcher();
    let status_watcher = ol_tracker.ol_status_watcher();
    let genesis_info = params.genesis_block_info();
    let genesis_blocknumhash =
        BlockNumHash::new(genesis_info.blockhash().0.into(), genesis_info.blocknum());

    let writer_config = Arc::new(WriterConfig {
        l1_fee_policy_config: sequencer_config.l1_fee_policy.clone(),
        ..Default::default()
    });
    log_writer_config(&writer_config);

    let block_builder_config =
        BlockBuilderConfig::default().with_blocktime_ms(sequencer_config.blocktime_ms);

    // Reads `SEQUENCER_PRIVATE_KEY` here rather than taking it from the
    // caller, which already read it to derive the gossip pubkey and failed
    // startup if it was missing or malformed — so this read cannot be the
    // first to fail. Keeping the two independent means splitting DA reveal
    // signing from gossip signing (see the note in `node.rs`) stays a local
    // change at each site.
    let sequencer_keypair = sequencer_bitcoin_keypair(&sequencer_privkey_from_env()?)?;

    let SequencerBootState {
        ol_chain_tracker: ol_chain_tracker_state,
        exec_chain: exec_chain_state,
        batch_builder: batch_builder_state,
        batch_lifecycle: batch_lifecycle_state,
    } = init_boot_state(storage.as_ref(), ol_client.as_ref()).await?;

    let payload_engine = Arc::new(AlpenRethPayloadEngine::new(
        payload_builder_handle,
        beacon_engine_handle,
        sequencer_config.beneficiary_address,
        storage.clone(),
    ));

    let exec_chain_handle = services::exec_chain::start_exec_chain_service(
        exec_chain_state,
        preconf_tx.clone(),
        storage.clone(),
        consensus_watcher.clone(),
        service_executor,
    )
    .instrument(info_span!("start_exec_chain", component = "alpen"))
    .await
    .map_err(|e| eyre::eyre!("failed to start exec chain service: {e}"))?;

    let (ol_chain_tracker, ol_chain_tracker_task) = build_ol_chain_tracker(
        ol_chain_tracker_state,
        status_watcher.clone(),
        ol_client.clone(),
        storage.clone(),
    );

    let (latest_batch, _) = require_latest_batch(storage.as_ref())
        .instrument(info_span!("require_latest_batch", component = "alpen"))
        .await?;

    let batch_sealing_policy =
        FixedBlockCountSealing::new(sequencer_config.batch_sealing_block_count);
    let block_data_provider = Arc::new(BlockCountDataProvider);

    // Per-block proof witnesses are captured inline during payload
    // build and persisted by `AlpenRethPayloadEngine`, and the
    // chunk prover's `ChunkSpec::fetch_input` assembles a chunk
    // proof input from those per-block records. There is no
    // chunk-seal extraction step and no chunk-spanning multiproof.

    // Channel from batch builder → chunk builder.
    let (batch_event_tx, batch_event_rx) =
        mpsc::channel::<BatchBuilderEvent>(sequencer_config.batch_event_channel_capacity);

    let (batch_builder_handle, batch_builder_task) = create_batch_builder(
        latest_batch.id(),
        genesis_blocknumhash,
        batch_builder_state,
        preconf_rx,
        block_data_provider,
        batch_sealing_policy,
        storage.clone(),
        storage.clone(),
        exec_chain_handle.clone(),
        Some(batch_event_tx),
    );

    let da_pipeline = da_pipeline::start(
        service_executor,
        &task_executor,
        da_pipeline::DaPipelineInputs {
            bitcoind: &sequencer_config.bitcoind,
            l1_reorg_safe_depth: sequencer_mode.l1_reorg_safe_depth,
            genesis_l1_height: sequencer_mode.genesis_l1_height,
            dbs,
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            params: params.clone(),
            writer_config,
            sequencer_keypair,
        },
    )
    .await?;

    let batch_prover = provers::launch(
        service_executor,
        dbs,
        ol_client.as_ref(),
        provers::EeProverInputs {
            storage: storage.clone(),
            node_provider: node_provider.clone(),
            btc_client: da_pipeline.btc_client,
            dev_native_prover: sequencer_config.dev_native_prover,
            sp1_deadline_secs: sequencer_config.sp1_proof_deadline_secs,
            params: params.clone(),
        },
    )
    .await?;

    let batch_da_provider = da_pipeline.batch_da_provider;

    let (batch_lifecycle_handle, batch_lifecycle_task) = create_batch_lifecycle_task(
        None,
        batch_lifecycle_state,
        batch_builder_handle.latest_batch_watcher(),
        batch_da_provider,
        batch_prover.clone(),
        storage.clone(),
    );

    let update_submitter_task = create_update_submitter_task(
        ol_client.clone(),
        storage.clone(),
        storage.clone(),
        batch_prover,
        batch_lifecycle_handle.latest_proof_ready_watcher(),
        status_watcher,
    );

    task_executor.spawn_critical(
        "ol_chain_tracker",
        ol_chain_tracker_task.instrument(info_span!("ol_chain_tracker", component = "alpen")),
    );
    // Per-block proof witnesses are captured inline during payload
    // build (in the EE node's `try_build_payload`) and persisted by
    // the payload engine (`AlpenRethPayloadEngine`) before the
    // payload is returned, so the block builder runs no separate
    // witness step. The chunk prover's `ChunkSpec::fetch_input`
    // assembles a chunk proof input from those per-block records.
    task_executor.spawn_critical(
        "block_assembly",
        block_builder_task(
            block_builder_config,
            exec_chain_handle,
            ol_chain_tracker,
            payload_engine,
            storage.clone(),
        )
        .instrument(info_span!("block_assembly", component = "alpen")),
    );

    // --- Chunk builder service ---
    let chunk_block_count = sequencer_config.chunk_sealing_block_count();

    // sequencer.chunk_sealing_gas_limit is validated against the genesis gas
    // limit in `node::launch`, before any node/DB/OL startup work.

    // u64::MAX effectively disables the gas policy while keeping a
    // single monomorphic code path (no dyn / enum branching).
    let chunk_gas_limit = sequencer_config.chunk_sealing_gas_limit.unwrap_or(u64::MAX);
    let chunk_sealing_policy = OrSealing::new(
        FixedBlockCountSealing::new(chunk_block_count),
        MaxGasSealing::new(chunk_gas_limit),
    );

    services::chunk_builder::start_chunk_builder_service(
        genesis_blocknumhash,
        storage.clone(),
        storage.clone(),
        storage.clone(),
        chunk_sealing_policy,
        RethGasDataProvider::new(node_provider.clone()),
        batch_event_rx,
        service_executor,
    )
    .await
    .map_err(|e| eyre::eyre!("failed to launch chunk builder service: {e}"))?;

    task_executor.spawn_critical(
        "ee_batch_builder",
        batch_builder_task.instrument(info_span!("ee_batch_builder", component = "alpen")),
    );
    task_executor.spawn_critical(
        "ee_batch_lifecycle",
        batch_lifecycle_task.instrument(info_span!("ee_batch_lifecycle", component = "alpen")),
    );
    task_executor.spawn_critical(
        "ee_update_submitter",
        update_submitter_task.instrument(info_span!("ee_update_submitter", component = "alpen")),
    );

    Ok(())
}
