# Alpen-Client: Setup and Operations Guide

## What is alpen-client

alpen-client is the Execution Environment (EE) node for Alpen — a custom Reth with:
- `alpen_gossip/1` RLPx subprotocol for real-time block header propagation between peers
- OL (Orchestration Layer) state tracking for finalized chain state
- Rollup-specific EVM precompiles

It runs in two modes: **sequencer** (produces blocks) or **fullnode** (follows the chain).

---

## Run as Sequencer

The sequencer builds EE blocks, signs block headers with Schnorr, and broadcasts them to all connected peers via the gossip protocol.

alpen-client takes only two Alpen-specific flags: `--alpen-params` (the chain/protocol artifact) and `--alpen-config` (this node's own TOML config — mode, OL connection, DA, sealing). Everything else is a Reth-native flag. `docker/alpen-client/entrypoint.sh` generates the `--alpen-config` file from env vars at container startup (see `docker/.env.alpen.example`); the same TOML can also be written by hand and passed directly if you're running the binary outside Docker.

### `--alpen-config` for a sequencer

```toml
l1_reorg_safe_depth = 6
genesis_l1_height = <height>
mode = "sequencer"

[ol]
source = "rpc"
client_url = "ws://<strata-host>:8432"
submit_url = "ws://<strata-host>:8435"
# bearer token is not a file field -- set STRATA_SUBMIT_RPC_TOKEN in the environment

[sequencer]
batch_sealing_block_count = 100

[sequencer.bitcoind]
rpc_url = "http://<bitcoind-host>:18443"
rpc_user = "<user>"
rpc_password = "<pass>"
network = "regtest"

[sequencer.l1_fee_policy]
fee_policy = "bitcoind"
```

### Required flags and env vars

```bash
SEQUENCER_PRIVATE_KEY=<32-byte-hex> \
STRATA_SUBMIT_RPC_TOKEN=<token> \
alpen-client \
  --datadir /data/sequencer \
  --alpen-config /path/to/alpen-config.toml \
  --alpen-params /path/to/alpen-params.json \
  --p2p-secret-key /path/to/p2p-secret.hex \
  --port 30303 \
  --addr 0.0.0.0 \
  --nat extip:<public-ip> \
  --disable-discovery \
  --trusted-peers <enode-urls> \
  --http --http.addr 0.0.0.0 --http.port 8545 --http.api eth,net,web3,txpool \
  --ws --ws.addr 0.0.0.0 --ws.port 8546 --ws.api eth,net,web3,txpool \
  --authrpc.addr 0.0.0.0 --authrpc.port 8551 --authrpc.jwtsecret /path/to/jwt.hex
```

### What each flag does

**Identity and role:**

| Flag / Env | Description |
|------------|-------------|
| `SEQUENCER_PRIVATE_KEY` | Env var. 32-byte hex Schnorr private key for signing gossip messages. Required when `--alpen-config`'s `mode = "sequencer"`. Accepts `0x` prefix. The gossip pubkey is derived from this key, not configured separately. |
| `STRATA_SUBMIT_RPC_TOKEN` | Env var. Bearer token for the OL submission RPC. Required when `mode = "sequencer"` and `ol.source = "rpc"`. Kept out of the config file since it's a secret. |
| `mode = "sequencer"` (`--alpen-config`) | Enables block building mode. Without this (i.e. `mode = "full_node"`), the node is a fullnode. Requires the `[sequencer]` table. |
| `full_node.sequencer_pubkey` | 32-byte x-only Schnorr public key. Only set on fullnode configs — the sequencer derives its own pubkey from `SEQUENCER_PRIVATE_KEY`, so it never configures one. Fullnodes need it to validate gossip signatures; it must match the sequencer's private key. |

**Chain and OL connection:**

| Flag | Description |
|------|-------------|
| `--alpen-params <path>` | Path to the Alpen params artifact (JSON). Carries the EE account id, bridge params, DA stream identity, and the embedded EVM chain spec. The pinned `strata-datatool` doesn't have `gen-alpen-params` yet, so until it ships, compose the JSON by hand: see the `AlpenParams` schema in `crates/alpen-ee/params/src/params.rs`, or `functional-tests/common/alpen_params.py` for a working example that stitches it together from `gen-ee-params` output and an in-repo chain spec. Required. |
| `--alpen-config <path>` | Path to this node's TOML config. Required. See the schema above and `bin/alpen-client/src/config.rs`. |
| `ol.client_url` (`[ol]`) | WebSocket or HTTP URL of the OL (strata) node. Example: `ws://strata:8432`. Required unless `ol.source = "dummy"`. |
| `ol.source = "dummy"` | Use a fake OL client. Only for isolated EE testing — not for production. |
| `--datadir <path>` | Where chain data, keys, and databases are stored. |

**P2P networking** (detailed in the P2P section below):

| Flag | Description |
|------|-------------|
| `--p2p-secret-key <file>` | Path to file containing 32-byte hex private key (no `0x` prefix). This key determines the node's enode identity. If the file doesn't exist, Reth auto-generates one. |
| `--port <port>` | P2P TCP listen port. Default: `30303`. |
| `--addr <ip>` | P2P listen address. Use `0.0.0.0` for all interfaces, `127.0.0.1` for loopback only. |
| `--nat <method>` | NAT traversal. Use `extip:<your-public-ip>` so the enode URL advertises the correct IP. Other options: `any`, `none`, `upnp`. |
| `--disable-discovery` | Disable DHT-based peer discovery. Peers must be configured statically. Recommended for controlled deployments. |
| `--trusted-peers <enodes>` | Comma-separated enode URLs. The node will actively connect to these peers on startup. |

**RPC endpoints:**

| Flag | Description |
|------|-------------|
| `--http` | Enable JSON-RPC over HTTP. |
| `--http.addr <ip>` | HTTP listen address. `0.0.0.0` for external access. |
| `--http.port <port>` | HTTP port. Default: `8545`. |
| `--http.api <apis>` | Comma-separated APIs to expose. Common: `eth,net,web3,txpool`. Add `admin,debug` if you need peer management or debugging. |
| `--ws` / `--ws.addr` / `--ws.port` / `--ws.api` | Same as HTTP but for WebSocket. |
| `--authrpc.addr` / `--authrpc.port` | Engine API endpoint (for OL ↔ EE communication). |
| `--authrpc.jwtsecret <file>` | JWT secret file for Engine API authentication. Shared between OL and this node. |

**DA pipeline** (`[sequencer]`/`[sequencer.bitcoind]`/`[sequencer.l1_fee_policy]` in `--alpen-config`, required when `mode = "sequencer"`):

| Field | Description |
|-------|-------------|
| `sequencer.bitcoind.rpc_url` / `rpc_user` / `rpc_password` | Bitcoin Core RPC endpoint and credentials. |
| `sequencer.bitcoind.network` | Bitcoin network (e.g. `regtest`, `signet`, `bitcoin`). Cross-checked against the connected bitcoind's own reported network at startup — alpen-client refuses to start on a mismatch. |
| `l1_reorg_safe_depth` (top-level) | Number of L1 confirmations before considering a block final. Default: `6`. Shared with fullnode configs — not sequencer-only. |
| `genesis_l1_height` (top-level) | The first L1 block height the rollup cares about. Default: `0`. Shared with fullnode configs — not sequencer-only. |
| `sequencer.batch_sealing_block_count` | Number of EE blocks per batch before sealing and posting DA. Default: `100`. Lower values seal more frequently. |
| `sequencer.l1_fee_policy.fee_policy` | Fee policy: `bitcoind`, `fixed`, or `mempool`. |

---

## Run as Fullnode

A fullnode validates gossip signatures, re-broadcasts to other peers, and optionally forwards user transactions to the sequencer.

### `--alpen-config` for a fullnode

```toml
l1_reorg_safe_depth = 6
genesis_l1_height = <height>
mode = "full_node"

[ol]
source = "rpc"
client_url = "ws://<strata-host>:8432"

[full_node]
sequencer_pubkey = "<same-pubkey-as-sequencer>"
sequencer_http_url = "http://<sequencer-host>:8545"  # omit for a read-only fullnode
```

```bash
alpen-client \
  --datadir /data/fullnode \
  --alpen-config /path/to/alpen-config.toml \
  --alpen-params /path/to/alpen-params.json \
  --p2p-secret-key /path/to/p2p-fn.hex \
  --port 30303 \
  --addr 0.0.0.0 \
  --nat extip:<public-ip> \
  --disable-discovery \
  --trusted-peers <enode-urls> \
  --http --http.addr 0.0.0.0 --http.port 8545 --http.api eth,net,web3,txpool \
  --ws --ws.addr 0.0.0.0 --ws.port 8546 --ws.api eth,net,web3,txpool \
  --authrpc.addr 0.0.0.0 --authrpc.port 8551 --authrpc.jwtsecret /path/to/jwt.hex
```

### Differences from sequencer

| | Sequencer | Fullnode |
|---|-----------|---------|
| `mode` (`--alpen-config`) | `"sequencer"` | `"full_node"` |
| `SEQUENCER_PRIVATE_KEY` env | Yes (signs gossip; pubkey derived from it) | No |
| `full_node.sequencer_pubkey` | Not set — derives its own from the private key | Yes (validates the sequencer's sigs) |
| `full_node.sequencer_http_url` | N/A | Optional — URL of sequencer's HTTP RPC. |
| `[sequencer]` table (DA, L1 posting) | Yes (posts state diffs to L1) | No |
| Block production | Yes | No — follows chain via gossip |

### Fullnode-specific field

| Field | Description |
|-------|-------------|
| `full_node.sequencer_http_url` | Sequencer's HTTP RPC URL (e.g. `http://sequencer:8545`). When a user sends a transaction to this fullnode, it gets forwarded to the sequencer for inclusion. A fullnode remains a valid read-only node without it — blocks arrive via gossip + Reth P2P regardless — it just can't accept writes. |

---

## P2P Networking

### Enode identity

Every node has an enode URL:
```
enode://<128-hex-char-pubkey>@<host>:<port>
```

The public key (64 bytes, 128 hex chars) is the **uncompressed** secp256k1 public key derived from the P2P secret key, with the `04` prefix stripped.

**Generating a P2P secret key:**
```bash
openssl rand -hex 32 > /path/to/p2p-secret.hex
```

The file must contain exactly 64 hex characters (no `0x` prefix, no newline). Reth reads this on startup. If the file doesn't exist, Reth generates a random key automatically (but then you can't predict the enode URL beforehand).

**Retrieving the enode URL at runtime:**
```bash
curl -s localhost:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"admin_nodeInfo","id":1}' | jq -r .result.enode
```

### Discovery modes

There are three ways to connect peers. Choose based on your deployment model.

#### Mode 1: Static peers, no discovery (recommended for production)

Simplest and most predictable. You pre-configure each node with the enode URLs of its peers.

```
--disable-discovery
--trusted-peers enode://<pubkey-A>@10.0.1.1:30303,enode://<pubkey-B>@10.0.1.2:30303
```

- `--disable-discovery` turns off all DHT/DNS discovery. The node won't look for peers on its own.
- `--trusted-peers` is a comma-separated list of enode URLs. The node actively connects to these on startup and maintains the connections.
- Each node's trusted-peers list should contain the other nodes (not itself).
- If a trusted peer goes down, the node retries periodically.

**When to use**: Production deployments where you control all nodes and know their IPs.

#### Mode 2: Runtime peer management via RPC

For dynamic setups where you want to add/remove peers without restarting.

```
--disable-discovery
--http.api eth,net,admin,debug    # Must include 'admin'
```

Then add peers at runtime:
```bash
# Add a peer
curl -s localhost:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"admin_addPeer","params":["enode://<pubkey>@<host>:30303"],"id":1}'

# Remove a peer
curl -s localhost:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"admin_removePeer","params":["enode://<pubkey>@<host>:30303"],"id":1}'
```

Can be combined with `--trusted-peers` for a baseline set of peers plus dynamic additions.

**When to use**: Environments where nodes come and go, or for debugging.

#### Mode 3: DiscV5 automatic discovery

Nodes discover each other via the discv5 protocol using bootnodes as entry points.

```
--disable-discv4-discovery
--enable-discv5-discovery
--discovery.v5.addr 0.0.0.0
--discovery.v5.port 30304
--bootnodes enode://<bootnode-pubkey>@<bootnode-host>:30303
```

- `--disable-discv4-discovery` disables the legacy discv4 protocol (only use discv5).
- `--enable-discv5-discovery` activates discv5.
- `--discovery.v5.addr` / `--discovery.v5.port` bind address/port for discv5 UDP.
- `--bootnodes` comma-separated enode URLs to bootstrap discovery. Typically the sequencer's enode.

Nodes will automatically discover and connect to other peers reachable through the bootnode graph. This forms a mesh topology — fullnodes connect to each other, not just to the sequencer.

**When to use**: Larger deployments where manual peer management is impractical, or when you want automatic mesh formation.

### Gossip protocol behavior

Once two alpen-client nodes are connected via P2P:

1. They negotiate the `alpen_gossip/1` RLPx subprotocol during the handshake.
2. If either side **doesn't** support `alpen_gossip/1`, the connection is **dropped** (`OnNotSupported::Disconnect`). This means plain Reth nodes cannot peer with alpen-client.
3. The sequencer broadcasts signed block headers to all connected peers whenever a new block is committed.
4. Fullnodes validate the Schnorr signature against `--sequencer-pubkey`, then re-broadcast to all their peers (except the sender). This is how blocks propagate through the mesh.
5. Duplicate/stale blocks (sequence number <= highest seen) are silently dropped.

### Network topology patterns

**Star (simplest)**: All fullnodes connect only to the sequencer.
```
fn1 ─→ seq ←─ fn2
         ↑
         fn3
```
Each fullnode has `--trusted-peers <seq-enode>`. Single point of failure for gossip relay.

**Mesh (recommended for resilience)**: Fullnodes also connect to each other.
```
fn1 ←──→ seq ←──→ fn2
  ↑                 ↑
  └────── fn3 ──────┘
```
Each node's `--trusted-peers` includes all other nodes (or use discv5 for automatic mesh formation). If one link goes down, blocks still propagate through alternate paths.

---

## Verifying P2P is Working

### 1. Check the gossip subprotocol registered

In the node's logs, look for:
```
INFO alpen-gossip: Registered Alpen gossip RLPx subprotocol
```
This appears once at startup. If missing, the binary was built without gossip support.

### 2. Check peer connections

```bash
# Peer count (hex-encoded)
curl -s localhost:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"net_peerCount","id":1}'
# Expected: {"result":"0x2"} for 2 peers

# Detailed peer list
curl -s localhost:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"admin_peers","id":1}' | jq '.result[] | {enode, name}'
```

If peer count is `0x0`:
- Verify enode URLs are correct (check public key and host/port).
- Verify nodes are on the same network and the P2P port is reachable.
- Verify `--nat` is set correctly — the advertised IP in the enode must be reachable by the other node.
- Check firewall rules on the P2P port (TCP).

### 3. Check gossip connections established

In the logs:
```
DEBUG alpen-gossip: New gossip connection established peer_id=<hex> direction=Inbound|Outbound
```
One log line per peer. If you see `Peer does not support alpen_gossip protocol, disconnecting` instead, the other node is not running alpen-client (or a version without gossip support).

### 4. Check block propagation

On the sequencer:
```
INFO alpen-gossip: Broadcasting new block to peers block_hash=0x... block_number=42 peer_count=2
```

On fullnodes:
```
INFO alpen-gossip: Received gossip package peer_id=<hex> block_hash=0x... seq_no=42
```

### 5. Compare block heights across nodes

```bash
for port in 8545 8555 8565; do
  echo -n "Port $port: "
  curl -s localhost:$port -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","id":1}' | jq -r .result
done
```

Fullnodes should be at the same height as (or very close to) the sequencer.

### 6. Verify block hash consistency

Same block number should have the same hash on all nodes:
```bash
BLOCK=0x10
for port in 8545 8555 8565; do
  echo -n "Port $port: "
  curl -s localhost:$port -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$BLOCK\",false],\"id\":1}" \
    | jq -r .result.hash
done
```

---

## Troubleshooting

**"SEQUENCER_PRIVATE_KEY environment variable is required"**
`--alpen-config`'s `mode = "sequencer"` but the env var is missing. Export it before starting the process.

**"sequencer feature not enabled at compile time"**
The binary was built without the `sequencer` cargo feature. Rebuild with `cargo build --bin alpen-client` (it's a default feature) or explicitly `-F sequencer`.

**"mode = \"sequencer\" requires the `sequencer` feature; rebuild with default features"**
`--alpen-config` asks for sequencer mode but this binary was built with `--no-default-features` (no `sequencer` feature). Either rebuild with the feature enabled, or point the config at `mode = "full_node"`.

**Peer count stuck at 0**
- Wrong enode URL (typo in pubkey, wrong host/port).
- `--nat` not set or set to wrong IP — the advertised enode IP must be reachable from the other node.
- Firewall blocking the P2P TCP port.
- If using `--disable-discovery` without `--trusted-peers`, no peers will be found. Use `admin_addPeer` or add `--trusted-peers`.

**"Peer does not support alpen_gossip protocol, disconnecting"**
The other node is vanilla Reth or an alpen-client built without gossip. All peers must run alpen-client with gossip support.

**"Received gossip package with invalid signature"**
The sequencer's `SEQUENCER_PRIVATE_KEY` doesn't match the `full_node.sequencer_pubkey` that fullnodes were given. Regenerate the keypair (the sequencer derives its pubkey from the private key, it doesn't take one directly) and ensure all fullnode configs use the resulting pubkey.

**"Received gossip package from unexpected public key"**
Same as above — the gossip message was signed with a key that doesn't match the configured `full_node.sequencer_pubkey`.

**Blocks not propagating to fullnodes**
- Verify the sequencer log shows "Broadcasting new block to peers" with `peer_count > 0`.
- Verify fullnode logs show gossip connection established.
- If the sequencer shows `peer_count=0`, the P2P connections exist but the gossip subprotocol didn't negotiate. Check both sides are running the same alpen-client version.

**"sequencer.bitcoind.network is configured as ..., but the connected bitcoind reports ..."**
`--alpen-config`'s `sequencer.bitcoind.network` doesn't match what the connected bitcoind actually reports (e.g. configured `regtest` but pointed at a `signet` node). Fix the `network` field or point `sequencer.bitcoind.rpc_url` at the right node.

**TOML parse error / "unknown field" on `--alpen-config`**
The config file doesn't match the schema. Check field names and table nesting against `bin/alpen-client/src/config.rs` or the checked-in examples at `bin/alpen-client/testdata/config.*.toml`.
