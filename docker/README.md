# Docker

Docker setups for the alpen-client (EE) node. The strata (OL) node images
live in the strata repo; local full-stack composes will return once the
post-split params/keys flow is in place (see the repo-split notes).

## Compose files

| Compose | Purpose |
|---|---|
| `compose-signet.yml` | Local signet `bitcoind` miner or fullnode |
| `docker-compose-da-verifier.yml` | Standalone EE DA verifier against staging-v2 RPC port-forwards |
| `docker-compose-eest.yml` | Ethereum execution spec test environment |
| `docker-compose-p2p-test.yml` | Minimal EE P2P/gossip test |

## Images

| Directory | Image |
|---|---|
| `alpen-client/` | EE node, plus the `dbconsole` database console (`Dockerfile` for CI/registry builds, `Dockerfile.local` for local compose builds) |
| `alpen-da-verifier/` | Standalone EE DA verifier |
| `bitcoind/` | Regtest bitcoind used by the test composes |

## EE DA verifier launcher

`alpen-da-verifier/entrypoint.sh` launches the verifier from mounted config
and params files. It requires Bitcoin RPC credentials and `GENESIS_L1_HEIGHT`;
the snapshot defaults to `$DATADIR/reconstruction.snapshot` and may be absent on
first launch.

| Variable | Default |
|---|---|
| `VERIFIER_CONFIG_PATH` | `/app/configs/da-verifier.toml` |
| `ALPEN_PARAMS_PATH` | `/app/configs/generated/alpen-params.json` |
| `DATADIR` | `/app/data` |
| `SNAPSHOT_PATH` | `$DATADIR/reconstruction.snapshot` |
| `GENESIS_L1_HEIGHT` | Required |
| `BITCOIND_RPC_USER` | Required |
| `BITCOIND_RPC_PASSWORD` | Required |

The verifier requires a real OL sequencer endpoint that retains EE account
update manifests and inner-state roots. The EEST and P2P composes use a dummy OL
client, so they cannot provide that dependency. This compose instead tests
against staging-v2 through local port-forwards. Its mounted TOML config must use:

```toml
ol_rpc_url = "http://host.docker.internal:57708"

[bitcoind]
rpc_url = "http://host.docker.internal:62744"
network = "signet"
```

The Bitcoin username and password are read from the process environment. The
Compose environment file is ignored by Git and excluded from the Docker build
context. A Kubernetes deployment uses the same image and entrypoint with
credentials sourced from a Secret and a separately mounted config containing
cluster-local Bitcoin and OL service URLs.

Create the ignored runtime config and Compose environment from their checked-in
templates. Set `BITCOIND_RPC_PASSWORD` in `.env.da-verifier` to the credential
used by the Bitcoin port-forward:

```bash
cp docker/configs/da-verifier-staging-v2.example.toml \
  docker/configs/da-verifier-staging-v2.toml
cp docker/.env.da-verifier.example docker/.env.da-verifier
```

The environment template mounts the repository's current `params/staging.json`.
Then run the verifier from the repository root:

```bash
docker compose --env-file docker/.env.da-verifier \
  -f docker/docker-compose-da-verifier.yml up --build
```

The compose defaults `GENESIS_L1_HEIGHT` to the current staging-v2 recovery
floor, `20919`. Override it together with the params file after a staging
regenesis.

There is no separate database container. The verifier opens its dedicated MDBX
environment under `/app/data`; the `verifier-data` volume retains that database
and the reconstruction snapshot across container restarts. Remove it explicitly
when a test requires a clean reconstruction:

```bash
docker compose --env-file docker/.env.da-verifier \
  -f docker/docker-compose-da-verifier.yml down --volumes
```

## Configs

`configs/` holds the `--alpen-config` TOML files the test composes mount into
their containers. The entrypoint does not build a config from environment
variables — it checks that the config and params files exist and then starts
the node.

See `operations.md` for alpen-client setup and operations, and
`../bin/alpen-client/README.md` for the config schema.
