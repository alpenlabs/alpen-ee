# Observability for alpen-client

**Local** metrics, a Grafana dashboard and (optionally) logs for one or more
`alpen-client` nodes. Intended to use as a dev-tooling.

```
docker compose -f docker/observability/compose.yml up -d
docker compose -f docker/observability/compose.yml --profile logs up -d   # with Loki + Alloy
```

Grafana: http://localhost:3000 (admin / admin), opening on the **Alpen EE
Service Map**. Prometheus: http://localhost:9090.

## What the node exposes

Every Alpen crate records through the `metrics` facade; the names and their
meaning are in each crate's `metrics.rs` (`crates/sequencer`, `crates/da/provider`,
`crates/engine/reth`, `crates/ol-tracker`, `bin/alpen-client`). The recorder is the
Prometheus one reth installs, so:

- the endpoint is reth's: start the node with `--metrics <addr>` (the docker
  entrypoint passes `METRICS_ADDR`, default `0.0.0.0:9001`; the functional tests
  pass a loopback port and put it in the service's `metrics_url` prop);
- every name is prefixed `reth_`: `alpen_ee_blocks_built_total` is scraped as
  `reth_alpen_ee_blocks_built_total`, next to reth's own metrics;
- histograms are rendered as summaries: `{quantile="0.5"}` series plus `_sum`
  and `_count` (Prometheus 3 shows the quantile labels as floats, `"1.0"`);
- a counter has no series until it first increments, so failure and retry
  series are absent on a healthy node rather than zero.

## Pointing Prometheus at a node

Targets are in `prometheus/targets/alpen.yml`, re-read every 30 s. The default
is the docker host at port 9001: what `docker/docker-compose-eest.yml` publishes
(`METRICS_HOST_PORT`; the image's entrypoint serves `--metrics` on `METRICS_ADDR`,
default `0.0.0.0:9001`), and what a native node started with
`--metrics 0.0.0.0:9001` serves (a node bound to `127.0.0.1` is not reachable from
a container). If the host port moves, move the target with it. For a node that
runs in another compose project, attach this stack to its network and use the
service name. A second node (a full node) is a second entry; the dashboard's
`node` picker switches between instances.

## The dashboard

`grafana/dashboards/alpen-ee-service-map.json` is **generated**: edit
`grafana/build_dashboard.py` and rerun it. The map's boxes and arrows are the
`SERVICES` and `FLOWS` tables at the top of the script, with the query behind
each figure; the sections below the map are built by `build()`. Grafana
provisions the file and overwrites UI edits on its next pass, so changes made
in the UI are lost unless exported back into the script.

The map is a canvas panel laid out for a window about 1200 px wide or more.

### The same dashboard in the fleet

`build_dashboard.py --target cluster` emits the dashboard as a ConfigMap for the
Grafana sidecar of a kube-prometheus-stack cluster (the deployments repo's
`clusters/<cluster>/values/dashboards/`). The EE pods there already serve
`--metrics` and are scraped by the strata-ee PodMonitor, so no collection
change is needed; the cluster form only swaps what differs:

- datasources become the `${prometheus_datasource}` / `${loki_datasource}`
  variables the fleet dashboards share;
- the node picker selects a `pod` in `$rollup_namespace` (every series the
  PodMonitor scrapes carries `namespace` and `pod`), so sequencer and fullnodes
  share one dashboard;
- the log panels read the OTel collector's `k8s_namespace_name` /
  `k8s_pod_name` streams;
- refresh is 30 s, the PodMonitor's scrape interval.

```
python3 docker/observability/grafana/build_dashboard.py --target cluster \
    --folder Testnet --out <deployments>/clusters/staging-v2/values/dashboards/testnet/ee-service-map.yaml
```

Regenerate the ConfigMaps whenever the generator changes; they are not edited
by hand.

## Driving a devnet

`functional-tests/run_tests.sh --keep-alive el_ol` starts the services and
leaves them alone: nobody mines regtest blocks, so OL epochs never advance,
DA never confirms and every batch sits in `da_pending`. Run the repo's driver
next to it (it mines and, unless told not to, submits a transaction trickle):

```
cd functional-tests && uv run python -m scripts.drive_keepalive --mine-interval 5 --tx-batch 0
```

## Logs

The `logs` profile runs Loki and an Alloy agent that ships every docker
container's stdout and stderr, labelled by compose service (`service=` the
compose service name, `container=` the container name). The dashboard's log
panels read `{service="alpen-client"}`, so name the node's compose service
`alpen-client` or edit the queries. A node that runs on the host writes its
log to a file; to ship that, add a `local.file_match` + `loki.source.file`
pair to `alloy/config.alloy` pointing at the mounted log directory, for
example for a keep-alive devnet (labels are what the dashboard's log panels
select on):

```
local.file_match "devnet" {
  path_targets = [
    { __path__ = "/data/el_ol/ee_sequencer/service.log",  service = "alpen-client", node = "ee_sequencer" },
    { __path__ = "/data/el_ol/ee_fullnode_0/service.log", service = "alpen-client", node = "ee_fullnode_0" },
  ]
}
loki.source.file "devnet" {
  targets       = local.file_match.devnet.targets
  forward_to    = [loki.write.local.receiver]
  tail_from_end = true
}
```

A `-vvvv` node writes about half a megabyte of log a second, so point Loki
at it with retention in mind. Running Loki as a single binary on a host (not
in compose) needs `frontend.address: 127.0.0.1` and
`query_scheduler.scheduler_ring.instance_addr: 127.0.0.1`, otherwise the
frontend advertises a non-loopback interface and every query hangs.

## Retention

Prometheus keeps 15 days (`PROMETHEUS_RETENTION`), Loki 7 days
(`loki/loki.yml`). Data lives in named docker volumes; `docker compose down -v`
clears it.
