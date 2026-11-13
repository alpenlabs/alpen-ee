//! P2P network setup that keeps Alpen discovery apart from Ethereum's.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use alloy_primitives::Bytes;
use reth_chainspec::{ForkId, Hardforks};
use reth_discv5::{
    discv5::{ListenConfig, ProtocolIdentity},
    Config as Discv5Config, ConfigBuilder as Discv5ConfigBuilder, NetworkStackId,
    DEFAULT_DISCOVERY_V5_PORT,
};
use reth_network::{
    primitives::BasicNetworkPrimitives, types::NatResolver, NetworkConfig, NetworkHandle,
    NetworkManager, NetworkPrimitives, PeersInfo,
};
use reth_node_api::{PrimitivesTy, TxTy};
use reth_node_builder::{
    components::NetworkBuilder,
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
};
use reth_node_core::args::{DiscoveryArgs, NetworkArgs};
use reth_transaction_pool::{PoolPooledTx, PoolTransaction, TransactionPool};
use tracing::{debug, info, warn};

/// The discv5 protocol identity of Alpen nodes.
///
/// Every discv5 packet carries it, and nodes ignore packets with any other
/// identity. Changing it splits the discovery network: nodes on the old and
/// new identity can no longer find each other.
pub const ALPEN_DISCV5_PROTOCOL: ProtocolIdentity = ProtocolIdentity {
    // The field is 6 bytes, and stock discv5 uses `discv5`. The `5` mirrors
    // that name; it is not a version.
    protocol_id: *b"alpen5",
    protocol_version: 1u16.to_be_bytes(),
};

/// Builds the P2P network with discovery isolated from Ethereum's.
///
/// Stock reth puts every node in Ethereum's discovery network: discv4 and
/// discv5 share Ethereum's DHT, and with no `--bootnodes` reth falls back to
/// Ethereum mainnet's bootnodes. An Alpen node then mostly finds and dials
/// Ethereum nodes, which all fail the handshake.
///
/// This builder runs only discv5, under [`ALPEN_DISCV5_PROTOCOL`]. Ethereum
/// nodes drop our packets and we drop theirs, so the discovery network holds
/// Alpen nodes only. Alpen networks (mainnet, testnet, ...) share that protocol
/// ID, so the node also advertises its fork ID under the standard `eth` key.
/// Reth drops discovered peers whose fork ID doesn't match its own.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct AlpenNetworkBuilder;

impl<Node, Pool> NetworkBuilder<Node, Pool> for AlpenNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
{
    type Network =
        NetworkHandle<BasicNetworkPrimitives<PrimitivesTy<Node::Types>, PoolPooledTx<Pool>>>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let args = &ctx.config().network;
        let config = isolate_discovery(ctx.network_config()?, args);

        if config.discovery_v5_config.is_none() && !args.discovery.disable_discovery {
            warn!(
                "discv5 is off and alpen-client never runs discv4, so this node has no \
                 discovery; it only connects to --trusted-peers and peers added over RPC"
            );
        } else if config.discovery_v5_config.is_some() && config.boot_nodes.is_empty() {
            if args.bootnodes.is_some() {
                warn!(
                    "none of --bootnodes resolved; this node only finds peers that contact it \
                     first or are set with --trusted-peers"
                );
            } else {
                warn!(
                    "discovery is on but --bootnodes is empty; this node only finds peers that \
                     contact it first or are set with --trusted-peers"
                );
            }
        }

        let network = NetworkManager::builder(config).await?;
        let handle = ctx.start_network(network, pool);
        info!(enode = %handle.local_node_record(), "P2P networking initialized");
        Ok(handle)
    }
}

/// Rewrites reth's default network config so discovery only reaches Alpen
/// nodes.
fn isolate_discovery<C, N: NetworkPrimitives>(
    mut config: NetworkConfig<C, N>,
    args: &NetworkArgs,
) -> NetworkConfig<C, N> {
    // discv4 has no way to keep a sub-network apart: it shares Ethereum's DHT
    // and only learns a node's fork ID after a separate ENR request.
    config.discovery_v4_config = None;

    // No DNS tree serves Alpen nodes, and the module only promises discv5.
    config.dns_discovery_config = None;

    // Without --bootnodes reth falls back to Ethereum mainnet's bootnodes.
    // With them, reth has already resolved them and dropped the ones it can't.
    if args.bootnodes.is_none() {
        config.boot_nodes.clear();
    }

    if config.discovery_v5_config.is_some() {
        // Rebuilt rather than patched, so it doesn't keep the bootnodes reth
        // already put in it. The network manager adds `config.boot_nodes`.
        let builder = discv5_on_rlpx_port(&args.discovery, config.listener_addr.port())
            .discovery_v5_builder(config.listener_addr, Vec::new());
        let discv5 = alpen_discv5_config(builder, config.fork_filter.current());
        let nat = (!args.discovery.disable_nat).then_some(&args.nat);
        config.discovery_v5_config = Some(with_external_ip(discv5, nat));
    }

    config
}

/// Moves discv5 onto the RLPx port, in place of reth's default UDP port.
///
/// An enode without `?discport=` sends discovery to its TCP port. With discv5
/// on reth's default port, a hand-written bootnode enode that leaves out
/// `?discport=` reaches nothing, and nothing logs it. discv4 is off, so the
/// RLPx port is free on UDP. geth also runs discovery on its RLPx port by
/// default.
///
/// Reth can't tell a port left at its default from one set to the same value,
/// so an explicit `--discovery.v5.port` equal to reth's default moves too.
fn discv5_on_rlpx_port(discovery: &DiscoveryArgs, rlpx_port: u16) -> DiscoveryArgs {
    let mut discovery = discovery.clone();
    if discovery.discv5_port == DEFAULT_DISCOVERY_V5_PORT {
        discovery.discv5_port = rlpx_port;
    }
    if discovery.discv5_port_ipv6 == DEFAULT_DISCOVERY_V5_PORT {
        discovery.discv5_port_ipv6 = rlpx_port;
    }
    discovery
}

/// Puts the `--nat` external IP in the discv5 record, before discv5 starts.
///
/// Reth only applies `--nat` to the discv4 record. The discv5 record takes its
/// IP from the listen address, so a node listening on `0.0.0.0` advertises no
/// IP, and other nodes can't reach it through records they get from discovery.
///
/// Upstream relies on discv5's IP voting instead: the record gets an IP once
/// 10 peers report the same address within two minutes. On Ethereum that
/// happens fast. A network as small as Alpen's may never get there, and reth
/// turns voting off whenever `--discovery.v5.addr` is set.
///
/// The IP only goes in when discv5 listens on the unspecified address of the
/// same family. A specific listen address is already in the record, and a
/// family discv5 doesn't listen on has no socket to reach. It warns when a
/// `--nat` setting the operator chose can't go in.
fn with_external_ip(mut config: Discv5Config, nat: Option<&NatResolver>) -> Discv5Config {
    let Some(nat) = nat else {
        return config;
    };
    let (ipv4, ipv6) = unspecified_listen_ports(&config.discv5_config_mut().listen_config);
    let Some(port) = ipv4.or(ipv6) else {
        if matches!(
            nat,
            NatResolver::ExternalIp(_) | NatResolver::ExternalAddr(_)
        ) {
            warn!(%nat, "ignoring --nat for discv5, which advertises its specific listen address");
        }
        return config;
    };

    // reth copies extra pairs into the record as is, so these set its address.
    let (key, ip): (&'static [u8], _) = match nat.clone().as_external_ip(port) {
        Some(IpAddr::V4(ip)) if ipv4.is_some() => (b"ip", ip.octets().to_vec()),
        Some(IpAddr::V6(ip)) if ipv6.is_some() => (b"ip6", ip.octets().to_vec()),
        Some(ip) => {
            warn!(
                %nat, %ip,
                "discv5 doesn't listen on this IP family, so its record has no external IP"
            );
            return config;
        }
        None => {
            match nat {
                NatResolver::ExternalAddr(_) => {
                    warn!(%nat, "couldn't resolve --nat, so the discv5 record has no external IP");
                }
                NatResolver::Upnp | NatResolver::PublicIp | NatResolver::NetIf => {
                    warn!(
                        %nat,
                        "discv5 only takes --nat extip or extaddr, so its record has no external IP"
                    );
                }
                _ => debug!(%nat, "the discv5 record has no external IP"),
            }
            return config;
        }
    };
    let value = Bytes::from(alloy_rlp::encode(ip.as_slice()));
    let config = Discv5ConfigBuilder::new_from(config)
        .add_enr_kv_pair(key, value)
        .build();
    info!(?nat, "advertising the external IP in the discv5 record");
    config
}

/// Returns the IPv4 and IPv6 ports discv5 listens on with the unspecified
/// address.
fn unspecified_listen_ports(listen: &ListenConfig) -> (Option<u16>, Option<u16>) {
    let ipv4 = |ip: &Ipv4Addr, port: u16| ip.is_unspecified().then_some(port);
    let ipv6 = |ip: &Ipv6Addr, port: u16| ip.is_unspecified().then_some(port);
    match listen {
        ListenConfig::Ipv4 { ip, port } => (ipv4(ip, *port), None),
        ListenConfig::Ipv6 { ip, port } => (None, ipv6(ip, *port)),
        ListenConfig::DualStack {
            ipv4: ip4,
            ipv4_port,
            ipv6: ip6,
            ipv6_port,
        } => (ipv4(ip4, *ipv4_port), ipv6(ip6, *ipv6_port)),
        _ => (None, None),
    }
}

/// Finishes a discv5 config with the Alpen protocol identity and fork ID.
///
/// The fork ID goes under the standard `eth` key. The protocol ID already hides
/// the record from Ethereum nodes, so a custom key would add nothing.
fn alpen_discv5_config(builder: Discv5ConfigBuilder, fork_id: ForkId) -> Discv5Config {
    let mut config = builder.fork(NetworkStackId::ETH, fork_id).build();
    config.discv5_config_mut().protocol_identity = ALPEN_DISCV5_PROTOCOL;
    config
}

#[cfg(test)]
mod tests {
    use std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    };

    use reth_chainspec::{ChainSpec, ForkHash, DEV};
    use reth_discv5::{discv5, Discv5};
    use reth_network::{config::rng_secret_key, EthNetworkPrimitives, NetworkConfigBuilder};
    use reth_storage_api::noop::NoopProvider;
    use reth_tasks::Runtime;

    use super::*;

    const BOOTNODE: &str = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@10.3.58.6:30303?discport=9200";

    const TEST_FORK_ID: ForkId = ForkId {
        hash: ForkHash([0x00, 0x5e, 0x23, 0x84]),
        next: 0,
    };

    const EXTERNAL_IPV4: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

    /// Builds a config the way reth does for a chain with no known bootnodes:
    /// discv4 and DNS discovery on, and the resolved `--bootnodes`, or
    /// Ethereum mainnet's bootnodes when there are none.
    fn stock_config(
        args: &NetworkArgs,
        discv5: bool,
    ) -> NetworkConfig<NoopProvider<ChainSpec>, EthNetworkPrimitives> {
        let rlpx_socket = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 30303);
        let builder =
            NetworkConfigBuilder::<EthNetworkPrimitives>::new(rng_secret_key(), Runtime::test());
        let builder = match args.resolved_bootnodes() {
            Some(boot_nodes) => builder.boot_nodes(boot_nodes),
            None => builder.mainnet_boot_nodes(),
        };
        let builder = if discv5 {
            builder.discovery_v5(Discv5Config::builder(rlpx_socket))
        } else {
            builder
        };
        builder.build_with_noop_provider(Arc::clone(&DEV))
    }

    #[tokio::test]
    async fn isolate_discovery_leaves_only_discv5() {
        let args = NetworkArgs::default();
        let mut config = isolate_discovery(stock_config(&args, true), &args);

        assert!(config.discovery_v4_config.is_none());
        assert!(config.dns_discovery_config.is_none());
        assert!(config.boot_nodes.is_empty(), "no Ethereum bootnodes");
        let discv5 = config
            .discovery_v5_config
            .as_mut()
            .expect("discv5 stays on");
        assert_eq!(
            discv5.discv5_config_mut().protocol_identity,
            ALPEN_DISCV5_PROTOCOL
        );
    }

    #[tokio::test]
    async fn isolate_discovery_keeps_given_bootnodes() {
        let args = NetworkArgs {
            bootnodes: Some(vec![BOOTNODE.parse().expect("valid enode")]),
            ..NetworkArgs::default()
        };

        let config = isolate_discovery(stock_config(&args, true), &args);

        assert_eq!(config.boot_nodes.len(), 1);
        assert_eq!(
            config.boot_nodes.iter().next().map(ToString::to_string),
            Some(BOOTNODE.to_owned())
        );
    }

    #[tokio::test]
    async fn isolate_discovery_leaves_discv5_off_when_disabled() {
        let args = NetworkArgs::default();
        let config = isolate_discovery(stock_config(&args, false), &args);

        assert!(config.discovery_v4_config.is_none());
        assert!(config.discovery_v5_config.is_none());
    }

    /// Returns the IPv4 port discv5 listens on.
    fn discv5_ipv4_port(config: &mut Discv5Config) -> Option<u16> {
        match config.discv5_config_mut().listen_config {
            ListenConfig::Ipv4 { port, .. } => Some(port),
            ListenConfig::DualStack { ipv4_port, .. } => Some(ipv4_port),
            _ => None,
        }
    }

    #[tokio::test]
    async fn isolate_discovery_puts_discv5_on_the_rlpx_port_by_default() {
        let args = NetworkArgs::default();
        let mut config = isolate_discovery(stock_config(&args, true), &args);

        let rlpx_port = config.listener_addr.port();
        let discv5 = config
            .discovery_v5_config
            .as_mut()
            .expect("discv5 stays on");
        assert_eq!(discv5_ipv4_port(discv5), Some(rlpx_port));
    }

    #[tokio::test]
    async fn isolate_discovery_keeps_an_explicit_discv5_port() {
        let mut args = NetworkArgs::default();
        args.discovery.discv5_port = 9300;
        let mut config = isolate_discovery(stock_config(&args, true), &args);

        let discv5 = config
            .discovery_v5_config
            .as_mut()
            .expect("discv5 stays on");
        assert_eq!(discv5_ipv4_port(discv5), Some(9300));
    }

    fn unused_udp_port() -> u16 {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|socket| socket.local_addr())
            .expect("bind a free UDP port")
            .port()
    }

    /// Builds the discv5 config of a node listening on `listen_ip:port`.
    /// `alpen` picks the Alpen protocol identity; otherwise the node runs
    /// stock discv5 with the same fork ID.
    fn node_config(listen_ip: IpAddr, port: u16, alpen: bool) -> Discv5Config {
        let listen = SocketAddr::new(listen_ip, port);
        let rlpx_socket = SocketAddr::new(listen_ip, 30303);
        let builder = Discv5Config::builder(rlpx_socket)
            .discv5_config(discv5::ConfigBuilder::new(ListenConfig::from(listen)).build());
        if alpen {
            alpen_discv5_config(builder, TEST_FORK_ID)
        } else {
            builder.fork(NetworkStackId::ETH, TEST_FORK_ID).build()
        }
    }

    /// Starts discv5 with the config `make` builds for a free port. Another
    /// test process can take the port before discv5 binds it, so it retries.
    async fn start(make: impl Fn(u16) -> Discv5Config) -> Discv5 {
        for _ in 0..5 {
            if let Ok((node, _events)) =
                Discv5::start(&rng_secret_key(), make(unused_udp_port())).await
            {
                return node;
            }
        }
        panic!("could not start discv5 on a free port");
    }

    async fn start_node(alpen: bool) -> Discv5 {
        start(|port| node_config(Ipv4Addr::LOCALHOST.into(), port, alpen)).await
    }

    async fn can_ping(from: &Discv5, to: &Discv5) -> bool {
        let to_enr = to.local_enr();
        from.with_discv5(|discv5| discv5.send_ping(to_enr))
            .await
            .is_ok()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alpen_nodes_reach_each_other_and_share_fork_id() {
        let node_a = start_node(true).await;
        let node_b = start_node(true).await;

        assert!(can_ping(&node_a, &node_b).await);
        assert_eq!(
            node_a.get_fork_id(&node_b.local_enr()).ok(),
            Some(TEST_FORK_ID)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alpen_and_stock_discv5_nodes_cannot_reach_each_other() {
        let alpen = start_node(true).await;
        let stock = start_node(false).await;

        assert!(!can_ping(&alpen, &stock).await);
        assert!(!can_ping(&stock, &alpen).await);
    }

    /// Starts an Alpen node listening on `listen_ip` with `--nat` set to `nat`.
    async fn start_with_nat(listen_ip: IpAddr, nat: Option<NatResolver>) -> Discv5 {
        start(|port| with_external_ip(node_config(listen_ip, port, true), nat.as_ref())).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn external_ip_fills_in_an_unspecified_listen_address() {
        let nat = NatResolver::ExternalIp(EXTERNAL_IPV4.into());
        let node = start_with_nat(Ipv4Addr::UNSPECIFIED.into(), Some(nat)).await;

        let enr = node.local_enr();
        assert_eq!(enr.ip4(), Some(EXTERNAL_IPV4));
        assert_eq!(enr.udp4(), Some(node.local_port()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn external_ip_keeps_a_specific_listen_address() {
        let nat = NatResolver::ExternalIp(EXTERNAL_IPV4.into());
        let node = start_with_nat(Ipv4Addr::LOCALHOST.into(), Some(nat)).await;

        assert_eq!(node.local_enr().ip4(), Some(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn external_ip_skips_a_family_discv5_does_not_listen_on() {
        let nat = NatResolver::ExternalIp(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into());
        let node = start_with_nat(Ipv4Addr::UNSPECIFIED.into(), Some(nat)).await;

        let enr = node.local_enr();
        assert_eq!(enr.ip4(), None);
        assert_eq!(enr.ip6(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn isolate_discovery_advertises_external_ip_with_unspecified_v5_addr() {
        let mut args = NetworkArgs {
            nat: NatResolver::ExternalIp(EXTERNAL_IPV4.into()),
            ..NetworkArgs::default()
        };
        args.discovery.discv5_addr = Some(Ipv4Addr::UNSPECIFIED);

        let node = start(|port| {
            let mut args = args.clone();
            args.discovery.discv5_port = port;
            let config = isolate_discovery(stock_config(&args, true), &args);
            config.discovery_v5_config.expect("discv5 stays on")
        })
        .await;

        assert_eq!(node.local_enr().ip4(), Some(EXTERNAL_IPV4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn external_ip_is_skipped_when_nat_is_disabled() {
        let node = start_with_nat(Ipv4Addr::UNSPECIFIED.into(), None).await;

        assert_eq!(node.local_enr().ip4(), None);
    }
}
