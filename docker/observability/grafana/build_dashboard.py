#!/usr/bin/env python3
"""Generates dashboards/alpen-ee-service-map.json.

The dashboard is a service map of alpen-client (a canvas panel: one box per
task or service, one arrow per data flow, each carrying a live figure) followed
by the sections of the pipeline: where a batch spends its time, block
production, consensus and finality, DA on Bitcoin, proving, OL submission,
the store, gossip and logs.

Edit SERVICES, FLOWS or the panel sections below and rerun:

    python3 build_dashboard.py                      # compose stack JSON
    python3 build_dashboard.py --target cluster     # fleet ConfigMap YAML

The cluster form is the same dashboard for a kube-prometheus-stack Grafana:
the Prometheus and Loki datasources become the `${prometheus_datasource}` and
`${loki_datasource}` variables the fleet's dashboards declare, the node picker
selects a `pod` in the rollup namespace (the strata-ee PodMonitor labels every
series with `namespace` and `pod`), and the log panels select the OTel
collector's `k8s_namespace_name` / `k8s_pod_name` streams. It is written as a
ConfigMap the Grafana sidecar picks up, for `clusters/<cluster>/values/dashboards/`
in the deployments repo.

Every query reads the node's own metrics (crates' `metrics.rs` files list
them). The reth recorder prefixes names with `reth_` and renders histograms
as summaries, so a p50 is `{quantile="0.5"}` and an average is
`rate(_sum)/rate(_count)`. Grafana provisions the JSON from
docker/observability/compose.yml; edits made in the Grafana UI are lost on the
next provisioning pass, so make them here.
"""

import argparse
import json
from pathlib import Path

OUT = Path(__file__).parent / "dashboards" / "alpen-ee-service-map.json"
DS = {"type": "prometheus", "uid": "alpen-prometheus"}
LOKI = {"type": "loki", "uid": "alpen-loki"}
P = "reth_alpen_ee_"  # every Alpen metric as the reth recorder exposes it
I = '{instance=~"$instance"}'  # the node picked at the top of the dashboard
RATE = "[$__rate_interval]"

# ---------------------------------------------------------------- the map ---

LANES = {
    "external": {"color": "#5b6370", "fill": "#2a2f38"},
    "consensus": {"color": "#4f86d8", "fill": "#1f3350"},
    "pipeline": {"color": "#4fa36a", "fill": "#1f3d2a"},
    "settlement": {"color": "#d08a3a", "fill": "#4a331a"},
    "prover": {"color": "#9a7ad8", "fill": "#36284f"},
    "reth": {"color": "#3fb3b3", "fill": "#1b3d3d"},
}

# Columns and rows of the map grid, in px inside the canvas.
COLS = [20, 225, 430, 635, 840, 1045]
ROWS = [20, 150, 280, 430]
BOX_W, BOX_H = 140, 58

# id, name, subtitle, lane, (col, row), headline query, caption, unit, decimals
SERVICES = [
    ("ol_node", "OL node", "strata, JSON-RPC", "external", (0, 1),
     f"sum(rate({P}ol_tracker_actions_total{I}{RATE}))", "polls/s", "short", 1),
    ("ol_tracker", "OL Tracker", "ol_tracker", "consensus", (1, 0),
     f"rate({P}ol_epochs_applied_total{I}{RATE}) * 60", "epochs/min", "short", 1),
    ("engine_control", "Engine Control", "engine_control", "consensus", (2, 0),
     f"sum(rate({P}forkchoice_updates_total{I}{RATE}))", "fcu/s", "short", 2),
    ("alpen_reth", "Alpen Reth", "engine::tree + exex", "reth", (3, 0),
     f'{P}payload_build_seconds{{instance=~"$instance",stage="total",quantile="0.5"}}', "build p50", "s", 3),
    ("gossip", "Gossip", "gossip_task", "consensus", (4, 0),
     f"rate({P}gossip_broadcasts_total{I}{RATE})", "headers/s", "short", 2),
    ("peers", "P2P peers", "RLPx alpen_gossip/1", "external", (5, 0),
     f"{P}gossip_peers{I}", "connected", "short", 0),
    ("ol_chain_tracker", "OL Chain Tracker", "ol_chain_tracker", "pipeline", (1, 1),
     f"increase({P}block_inbox_messages_total{I}[1h])", "deposits/h", "short", 0),
    ("block_builder", "Block Builder", "block_assembly", "pipeline", (2, 1),
     f"rate({P}blocks_built_total{I}{RATE})", "blocks/s", "short", 2),
    ("exec_chain", "Exec Chain", "exec_chain", "pipeline", (3, 1),
     f'{P}block_latest_height{{instance=~"$instance",status="head"}}', "head", "none", 0),
    ("batch_builder", "Batch Builder", "ee_batch_builder", "pipeline", (4, 1),
     f"sum(rate({P}batches_sealed_total{I}{RATE})) * 60", "batches/min", "short", 1),
    ("chunk_builder", "Chunk Builder", "chunk_builder", "pipeline", (5, 1),
     f"sum(rate({P}chunks_sealed_total{I}{RATE})) * 60", "chunks/min", "short", 1),
    ("batch_lifecycle", "Batch Lifecycle", "ee_batch_lifecycle", "settlement", (4, 2),
     f'{P}batch_latest_idx{{instance=~"$instance",status="proof_ready"}}', "proof-ready", "none", 0),
    ("da_pipeline", "DA Pipeline", "l1_broadcaster + watcher", "settlement", (3, 2),
     f"rate({P}da_envelopes_submitted_total{I}{RATE}) * 60", "envelopes/min", "short", 1),
    ("prover", "Prover", "prover", "prover", (5, 2),
     f'rate({P}proof_receipts_total{{instance=~"$instance",kind="acct"}}{RATE}) * 60', "acct/min", "short", 1),
    ("bitcoind", "Bitcoin node", "RPC", "external", (3, 3),
     f"sum(rate({P}da_status_checks_total{I}{RATE}))", "L1 polls/s", "short", 2),
    ("update_submitter", "Update Submitter", "ee_update_submitter", "settlement", (4, 3),
     f"sum(rate({P}ol_updates_submitted_total{I}{RATE})) * 60", "updates/min", "short", 1),
    ("prover_net", "Prover backend", "native / SP1", "external", (5, 3),
     f"sum(rate({P}proof_receipts_total{I}{RATE})) * 60", "receipts/min", "short", 1),
]

# id, from, to, query, caption, unit, decimals[, where along the arrow the
# label sits, 0..1 from the source; default 0.5]
FLOWS = [
    ("ol_to_tracker", "ol_node", "ol_tracker",
     f'{P}ol_epoch{{instance=~"$instance",status="local"}}', "epoch applied", "none", 0),
    ("tracker_to_engine", "ol_tracker", "engine_control",
     f'{P}block_latest_height{{instance=~"$instance",status="head"}} - ignoring(status) {P}block_latest_height{{instance=~"$instance",status="finalized"}}', "finality lag", "short", 0),
    ("tracker_to_olct", "ol_tracker", "ol_chain_tracker",
     f'{P}ol_epoch{{instance=~"$instance",status="latest"}}', "latest epoch", "none", 0),
    ("olct_to_bb", "ol_chain_tracker", "block_builder",
     f"increase({P}block_inbox_messages_total{I}[1h])", "deposits/h", "short", 0),
    ("bb_to_engine", "block_builder", "alpen_reth",
     f'{P}payload_build_seconds{{instance=~"$instance",stage="total",quantile="0.5"}}', "build p50", "s", 3, 0.75),
    ("bb_to_exec", "block_builder", "exec_chain",
     f"rate({P}blocks_built_total{I}{RATE})", "blocks/s", "short", 2),
    ("exec_to_engine", "exec_chain", "engine_control",
     f'{P}block_interval_seconds{{instance=~"$instance",quantile="0.5"}}', "interval p50", "s", 2, 0.3),
    ("engine_to_reth", "engine_control", "alpen_reth",
     f"sum(rate({P}forkchoice_updates_total{I}{RATE}))", "fcu/s", "short", 2),
    ("exec_to_gossip", "exec_chain", "gossip",
     f"rate({P}gossip_broadcasts_total{I}{RATE})", "headers/s", "short", 2),
    ("gossip_to_peers", "gossip", "peers",
     f"{P}gossip_peers{I}", "peers", "short", 0),
    ("exec_to_bb2", "exec_chain", "batch_builder",
     f'{P}batch_blocks{{instance=~"$instance",quantile="0.5"}}', "blocks/batch", "short", 0),
    ("bb2_to_chunk", "batch_builder", "chunk_builder",
     f"sum(rate({P}chunks_sealed_total{I}{RATE})) * 60", "chunks/min", "short", 1),
    ("bb2_to_life", "batch_builder", "batch_lifecycle",
     f"sum(rate({P}batches_sealed_total{I}{RATE})) * 60", "batches/min", "short", 1),
    ("chunk_to_prover", "chunk_builder", "prover",
     f'rate({P}proof_tasks_submitted_total{{instance=~"$instance",kind="chunk"}}{RATE}) * 60', "chunks/min", "short", 1),
    ("life_to_da", "batch_lifecycle", "da_pipeline",
     f'{P}da_blob_bytes{{instance=~"$instance",quantile="0.5"}}', "blob p50", "bytes", 0),
    ("da_to_btc", "da_pipeline", "bitcoind",
     f"rate({P}da_envelopes_submitted_total{I}{RATE}) * 120", "L1 txs/min", "short", 1),
    ("btc_to_da", "bitcoind", "da_pipeline",
     f'{P}batch_status_seconds{{instance=~"$instance",status="da_complete",quantile="0.5"}}', "seal→DA p50", "s", 1),
    ("life_to_prover", "batch_lifecycle", "prover",
     f'{P}proof_wait_seconds{{instance=~"$instance",quantile="0.5"}}', "proof wait p50", "s", 1),
    ("prover_to_backend", "prover", "prover_net",
     f"sum(rate({P}proof_receipts_total{I}{RATE})) * 60", "receipts/min", "short", 1),
    ("life_to_submit", "batch_lifecycle", "update_submitter",
     f'{P}batch_status_seconds{{instance=~"$instance",status="proof_ready",quantile="0.5"}}', "proving p50", "s", 1),
    ("submit_to_ol", "update_submitter", "ol_node",
     f"sum(rate({P}ol_updates_submitted_total{I}{RATE})) * 60", "updates/min", "short", 1),
]


def box_rect(col, row):
    return COLS[col], ROWS[row], BOX_W, BOX_H


def anchor(rect, other):
    """Where an arrow leaves `rect` towards `other`: the side facing it, in
    the canvas's normalized element coordinates (x right, y up, [-1, 1])."""
    x, y, w, h = rect
    ox, oy, ow, oh = other
    dx = (ox + ow / 2) - (x + w / 2)
    dy = (oy + oh / 2) - (y + h / 2)
    if abs(dx) * h > abs(dy) * w:  # mostly horizontal
        return (1.0 if dx > 0 else -1.0, 0.0)
    return (0.0, -1.0 if dy > 0 else 1.0)  # mostly vertical (canvas y is up)


def point(rect, a):
    x, y, w, h = rect
    return x + w / 2 + a[0] * w / 2, y + h / 2 - a[1] * h / 2


def text_el(name, text, rect, size=13, color="#e6e9ef", align="center", valign="middle",
            fill=None, border=None):
    x, y, w, h = rect
    el = {
        "type": "text", "name": name,
        "config": {"text": {"mode": "fixed", "fixed": text}, "size": size,
                   "color": {"fixed": color}, "align": align, "valign": valign},
        "background": {"color": {"fixed": fill or "transparent"}},
        "border": {"color": {"fixed": border or "transparent"}, "width": 1 if border else 0, "radius": 6},
        "constraint": {"horizontal": "left", "vertical": "top"},
        "placement": {"top": y, "left": x, "width": w, "height": h},
    }
    return el


def metric_el(name, field, rect, size=12, color="#ffffff", align="center", valign="middle"):
    x, y, w, h = rect
    return {
        "type": "metric-value", "name": name,
        "config": {"text": {"mode": "field", "field": field, "fixed": ""}, "size": size,
                   "color": {"fixed": color}, "align": align, "valign": valign},
        "background": {"color": {"fixed": "transparent"}},
        "border": {"color": {"fixed": "transparent"}, "width": 0},
        "constraint": {"horizontal": "left", "vertical": "top"},
        "placement": {"top": y, "left": x, "width": w, "height": h},
    }


def service_map_panel(y):
    rects = {s[0]: box_rect(*s[4]) for s in SERVICES}
    lanes = {s[0]: s[3] for s in SERVICES}
    elements, targets, overrides = [], [], []
    boxes = {}
    for sid, name, subtitle, lane, _, query, caption, unit, decimals in SERVICES:
        x, y0, w, h = rects[sid]
        c = LANES[lane]
        box = text_el(sid, name, (x, y0, w, h), size=13, align="center", valign="top",
                      fill=c["fill"], border=c["color"])
        boxes[sid] = box
        elements.append(box)
        elements.append(text_el(f"{sid}__sub", subtitle, (x + 4, y0 + 18, w - 8, 14), size=9,
                                color="#aab2bf"))
        elements.append(metric_el(f"{sid}__val", f"svc_{sid}", (x + 4, y0 + 34, w // 2 + 10, 20),
                                  size=13, align="left"))
        elements.append(text_el(f"{sid}__cap", caption, (x + w // 2 + 10, y0 + 37, w // 2 - 14, 16),
                                size=9, color="#aab2bf", align="right"))
        targets.append(target(query, f"svc_{sid}", instant=True))
        overrides.append(override(f"svc_{sid}", unit, decimals))

    for flow in FLOWS:
        fid, src, dst, query, caption, unit, decimals = flow[:7]
        at = flow[7] if len(flow) > 7 else 0.5
        a = anchor(rects[src], rects[dst])
        b = anchor(rects[dst], rects[src])
        # twin arrows between the same pair run side by side, one each way
        twin = any(f[1] == dst and f[2] == src for f in FLOWS)
        side = 0.35 if (src, dst) < (dst, src) else -0.35
        if twin:
            if a[1] == 0:
                a, b = (a[0], side), (b[0], side)
            else:
                a, b = (side, a[1]), (side, b[1])
        boxes[src].setdefault("connections", []).append({
            "source": {"x": a[0], "y": a[1]}, "target": {"x": b[0], "y": b[1]},
            "targetName": dst, "path": "straight",
            "color": {"fixed": LANES[lanes[src]]["color"]}, "size": {"fixed": 2},
        })
        (x1, y1), (x2, y2) = point(rects[src], a), point(rects[dst], b)
        mx, my = x1 + (x2 - x1) * at, y1 + (y2 - y1) * at
        lw, lh = 66, 30
        if abs(x2 - x1) > abs(y2 - y1) * 2:  # horizontal: label above the line
            lx, ly = mx - lw / 2, my - lh - 2
            if twin:  # the lower twin's label goes below its line
                ly = my - lh - 2 if side > 0 else my + 2
        elif abs(y2 - y1) > abs(x2 - x1) * 2:  # vertical: label beside the line
            lx, ly = mx + 5, my - lh / 2
            if twin:  # each twin's label on its outer side
                lx = mx + 5 if side > 0 else mx - lw - 5
        else:  # diagonal: label beside the point, vertically centred on it
            lx, ly = mx + 6, my - lh / 2
        elements.append(metric_el(f"{fid}__val", f"flow_{fid}", (lx, ly, lw, 16), size=11,
                                  color="#f2f4f7"))
        elements.append(text_el(f"{fid}__cap", caption, (lx, ly + 15, lw, 13), size=9,
                                color="#aab2bf"))
        targets.append(target(query, f"flow_{fid}", instant=True))
        overrides.append(override(f"flow_{fid}", unit, decimals))

    # lane legend
    lx = 20
    for lane, label in [("consensus", "consensus & sync"), ("pipeline", "block production"),
                        ("settlement", "settlement & DA"), ("prover", "proving"),
                        ("reth", "reth"), ("external", "external")]:
        elements.append(text_el(f"legend_{lane}", label, (lx, 500, 118, 16), size=10,
                                color="#dfe3ea", fill=LANES[lane]["fill"], border=LANES[lane]["color"]))
        lx += 126

    return {
        "type": "canvas", "title": "Service map", "id": next_id(),
        "description": "Every task and service in alpen-client with the external systems it "
                       "talks to. Arrows follow the data; each carries the figure the node "
                       "reports now. Rates are over the dashboard's rate interval.",
        "gridPos": {"x": 0, "y": y, "w": 24, "h": 18},
        "datasource": DS, "targets": targets,
        "fieldConfig": {"defaults": {"noValue": "no data", "color": {"mode": "fixed", "fixedColor": "#ffffff"},
                                     "unit": "short", "decimals": 1},
                        "overrides": overrides},
        "options": {
            "inlineEditing": False, "showAdvancedTypes": True, "panZoom": False,
            "infinitePan": False,
            "root": {"type": "frame", "name": "Root", "elements": elements,
                     "background": {"color": {"fixed": "transparent"}},
                     "border": {"color": {"fixed": "transparent"}, "width": 0},
                     "constraint": {"horizontal": "left", "vertical": "top"},
                     "placement": {"top": 0, "left": 0, "width": 1200, "height": 520}},
        },
    }


# --------------------------------------------------------- panel helpers ---

_ids = iter(range(1, 10000))


def next_id():
    return next(_ids)


def target(expr, legend, instant=False, ref=None):
    t = {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref or legend[:40],
         "instant": instant, "range": not instant, "editorMode": "code"}
    return t


def override(name, unit, decimals):
    return {"matcher": {"id": "byName", "options": name},
            "properties": [{"id": "unit", "value": unit}, {"id": "decimals", "value": decimals}]}


def grid(x, y, w, h):
    return {"x": x, "y": y, "w": w, "h": h}


def timeseries(title, targets, gp, unit="short", desc="", decimals=None, stack=False,
               fill=10, min0=True, legend="bottom", draw="line", overrides=None):
    custom = {"drawStyle": draw, "lineWidth": 1, "fillOpacity": fill, "showPoints": "never",
              "spanNulls": True, "stacking": {"mode": "normal" if stack else "none", "group": "A"}}
    defaults = {"unit": unit, "custom": custom, "color": {"mode": "palette-classic"}}
    if decimals is not None:
        defaults["decimals"] = decimals
    if min0:
        defaults["min"] = 0
    return {"type": "timeseries", "title": title, "id": next_id(), "description": desc,
            "gridPos": gp, "datasource": DS, "targets": targets,
            "fieldConfig": {"defaults": defaults, "overrides": overrides or []},
            "options": {"legend": {"displayMode": "list", "placement": legend, "showLegend": True},
                        "tooltip": {"mode": "multi", "sort": "desc"}}}


def stat(title, targets, gp, unit="short", desc="", decimals=None, color="blue", sparkline=True,
         thresholds=None):
    defaults = {"unit": unit, "color": {"mode": "fixed", "fixedColor": color}}
    if decimals is not None:
        defaults["decimals"] = decimals
    if thresholds:
        defaults["color"] = {"mode": "thresholds"}
        defaults["thresholds"] = {"mode": "absolute", "steps": thresholds}
    return {"type": "stat", "title": title, "id": next_id(), "description": desc,
            "gridPos": gp, "datasource": DS, "targets": targets,
            "fieldConfig": {"defaults": defaults, "overrides": []},
            "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                        "graphMode": "area" if sparkline else "none", "colorMode": "value",
                        "textMode": "value", "justifyMode": "center", "orientation": "auto"}}


def bargauge(title, targets, gp, unit="short", desc="", decimals=None, max_=None):
    defaults = {"unit": unit, "color": {"mode": "continuous-BlYlRd"}, "min": 0}
    if decimals is not None:
        defaults["decimals"] = decimals
    if max_ is not None:
        defaults["max"] = max_
    return {"type": "bargauge", "title": title, "id": next_id(), "description": desc,
            "gridPos": gp, "datasource": DS, "targets": targets,
            "fieldConfig": {"defaults": defaults, "overrides": []},
            "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                        "orientation": "horizontal", "displayMode": "gradient",
                        "showUnfilled": True, "valueMode": "color", "namePlacement": "auto",
                        "sizing": "auto"}}


def table(title, targets, gp, desc="", transformations=None, overrides=None):
    return {"type": "table", "title": title, "id": next_id(), "description": desc,
            "gridPos": gp, "datasource": DS, "targets": targets,
            "transformations": transformations or [],
            "fieldConfig": {"defaults": {"custom": {"align": "auto", "cellOptions": {"type": "auto"}}},
                            "overrides": overrides or []},
            "options": {"showHeader": True, "cellHeight": "sm",
                        "footer": {"show": False, "reducer": ["sum"], "fields": ""}}}


def logs(title, expr, gp, desc=""):
    return {"type": "logs", "title": title, "id": next_id(), "description": desc,
            "gridPos": gp, "datasource": LOKI,
            "targets": [{"datasource": LOKI, "expr": expr, "refId": "A", "queryType": "range"}],
            "options": {"showTime": True, "wrapLogMessage": True, "prettifyLogMessage": False,
                        "enableLogDetails": True, "dedupStrategy": "none", "sortOrder": "Descending",
                        "showLabels": False, "showCommonLabels": False}}


def row(title, y, collapsed=False, panels=None):
    r = {"type": "row", "title": title, "id": next_id(), "collapsed": collapsed,
         "gridPos": {"x": 0, "y": y, "w": 24, "h": 1}, "panels": panels or []}
    return r


def q(metric_suffix, extra="", legend=None, instant=False):
    """A series of one Alpen metric for the selected node."""
    sel = 'instance=~"$instance"' + (", " + extra if extra else "")
    return target(f"{P}{metric_suffix}{{{sel}}}", legend or metric_suffix, instant=instant)


def rate_q(metric_suffix, legend, extra="", mul=None, by=None):
    sel = 'instance=~"$instance"' + (", " + extra if extra else "")
    expr = f"rate({P}{metric_suffix}{{{sel}}}{RATE})"
    if by:
        expr = f"sum by ({by}) ({expr})"
    if mul:
        expr = f"({expr}) * {mul}"
    return target(expr, legend)


def avg_q(metric_suffix, legend, extra=""):
    sel = 'instance=~"$instance"' + (", " + extra if extra else "")
    return target(f"rate({P}{metric_suffix}_sum{{{sel}}}{RATE}) / rate({P}{metric_suffix}_count{{{sel}}}{RATE})",
                  legend)


def quant(metric_suffix, quantile, legend, extra=""):
    # Prometheus 3 normalises quantile labels to floats ("1.0"); 2.x keeps "1".
    qsel = f'quantile=~"{quantile}|{quantile}.0"' if quantile in ("0", "1") else f'quantile="{quantile}"'
    sel = f'instance=~"$instance", {qsel}' + (", " + extra if extra else "")
    return target(f"{P}{metric_suffix}{{{sel}}}", legend)


# ------------------------------------------------------------- sections ---

def build():
    panels = []
    y = 0

    # headline tiles
    tiles = [
        ("Head height", [q("block_latest_height", 'status="head"', "head")], "none", "blue", 0),
        ("Finalized height", [q("block_latest_height", 'status="finalized"', "finalized")], "none", "green", 0),
        ("Finality lag (blocks)", [target(f'{P}block_latest_height{{instance=~"$instance",status="head"}} - ignoring(status) {P}block_latest_height{{instance=~"$instance",status="finalized"}}', "lag")], "none", "orange", 0),
        ("Latest sealed batch", [q("batch_latest_idx", 'status="sealed"', "sealed")], "none", "blue", 0),
        ("OL latest batch", [q("batch_latest_idx", 'status="ol_latest"', "ol_latest")], "none", "purple", 0),
        ("Settlement p50", [quant("batch_settlement_seconds", "0.5", "p50")], "s", "orange", 1),
    ]
    for i, (title, targets, unit, color, dec) in enumerate(tiles):
        panels.append(stat(title, targets, grid(i * 4, y, 4, 4), unit=unit, color=color, decimals=dec))
    y += 4

    panels.append(service_map_panel(y))
    y += 18

    # --- where a batch spends its time
    panels.append(row("Where a batch spends its time", y)); y += 1
    stage_desc = ("Seconds a batch spent in the stage it left. da_complete: sealed until the DA envelope "
                  "finalized on L1. proof_pending: DA finalized until the account proof task was handed "
                  "over. proof_ready: proving. The clock forgets stages a restart interrupted.")
    panels.append(bargauge("Stage duration, p50", [
        quant("batch_status_seconds", "0.5", "seal → DA finalized", 'status="da_complete"'),
        quant("batch_status_seconds", "0.5", "DA → proof tasks", 'status="proof_pending"'),
        quant("batch_status_seconds", "0.5", "proving", 'status="proof_ready"'),
        quant("batch_settlement_seconds", "0.5", "whole settlement"),
    ], grid(0, y, 8, 8), unit="s", desc=stage_desc, decimals=1))
    panels.append(bargauge("Stage duration, p90", [
        quant("batch_status_seconds", "0.9", "seal → DA finalized", 'status="da_complete"'),
        quant("batch_status_seconds", "0.9", "DA → proof tasks", 'status="proof_pending"'),
        quant("batch_status_seconds", "0.9", "proving", 'status="proof_ready"'),
        quant("batch_settlement_seconds", "0.9", "whole settlement"),
    ], grid(8, y, 8, 8), unit="s", desc=stage_desc, decimals=1))
    panels.append(timeseries("Settlement time over time", [
        quant("batch_settlement_seconds", "0.5", "p50"),
        quant("batch_settlement_seconds", "0.9", "p90"),
        quant("batch_settlement_seconds", "0.99", "p99"),
    ], grid(16, y, 8, 8), unit="s", desc="Seconds from a sealed batch first seen by the lifecycle to its proof being ready.", decimals=1))
    y += 8
    panels.append(timeseries("Batch frontier by stage", [
        q("batch_latest_idx", 'status="sealed"', "sealed"),
        q("batch_latest_idx", 'status="da_pending"', "da_pending"),
        q("batch_latest_idx", 'status="da_complete"', "da_complete"),
        q("batch_latest_idx", 'status="proof_pending"', "proof_pending"),
        q("batch_latest_idx", 'status="proof_ready"', "proof_ready"),
        q("batch_latest_idx", 'status="ol_latest"', "ol_latest (included at OL tip)"),
    ], grid(0, y, 12, 8), desc="Index of the latest batch at each lifecycle stage. Lines that fan out are a stage falling behind.", fill=0, decimals=0))
    panels.append(timeseries("Batch stage transitions and failures", [
        rate_q("batch_transitions_total", "{{status}} entered", mul=60, by="status"),
        rate_q("batch_transition_failures_total", "{{status}} failed", mul=60, by="status"),
    ], grid(12, y, 12, 8), unit="short", desc="Per minute. Failures are lifecycle steps that errored and will be retried.", decimals=1))
    y += 8

    # --- block production
    panels.append(row("Block production", y)); y += 1
    panels.append(timeseries("Block interval", [
        quant("block_interval_seconds", "0.5", "p50"),
        quant("block_interval_seconds", "0.9", "p90"),
        quant("block_interval_seconds", "0.99", "p99"),
        quant("block_interval_seconds", "1", "max"),
    ], grid(0, y, 8, 8), unit="s", desc="Seconds between a block's timestamp and its parent's. Target: the configured block time.", decimals=2, min0=False))
    panels.append(timeseries("Payload build time, p50 by phase", [
        quant("payload_build_seconds", "0.5", "resolve (reth builds)", 'stage="resolve"'),
        quant("payload_build_seconds", "0.5", "accept (engine newPayload)", 'stage="accept"'),
        quant("payload_build_seconds", "0.5", "persist (witness stored)", 'stage="persist"'),
        quant("payload_build_seconds", "0.5", "total", 'stage="total"'),
        quant("block_build_seconds", "0.5", "block task total", 'stage="total"'),
    ], grid(8, y, 8, 8), unit="s", desc="Inside reth's payload builder and the block task around it.", decimals=3))
    panels.append(timeseries("Blocks built, failures and retries", [
        rate_q("blocks_built_total", "built /min", mul=60),
        rate_q("block_build_failures_total", "failed: {{reason}} /min", mul=60, by="reason"),
        rate_q("payload_build_retries_total", "payload retries /min", mul=60),
        rate_q("payload_build_failures_total", "payload failed: {{reason}} /min", mul=60, by="reason"),
    ], grid(16, y, 8, 8), desc="Per minute.", decimals=1))
    y += 8
    panels.append(timeseries("Payload size", [
        quant("block_payload_bytes", "0.5", "p50"),
        quant("block_payload_bytes", "0.99", "p99"),
    ], grid(0, y, 8, 7), unit="bytes", desc="Encoded execution payload per block."))
    panels.append(timeseries("Deposits consumed", [
        rate_q("block_inbox_messages_total", "inbox messages /min", mul=60),
    ], grid(8, y, 8, 7), desc="OL inbox messages the block builder consumed, per minute.", decimals=1))
    panels.append(timeseries("Blocks per batch and chunks", [
        quant("batch_blocks", "0.5", "blocks per batch p50"),
        rate_q("batches_sealed_total", "batches sealed: {{reason}} /min", mul=60, by="reason"),
        rate_q("chunks_sealed_total", "chunks sealed: {{reason}} /min", mul=60, by="reason"),
        rate_q("batch_reorgs_total", "{{kind}} reorgs /min", mul=60, by="kind"),
    ], grid(16, y, 8, 7), desc="Sealing policy at work: reason is the policy that fired.", decimals=1, fill=0))
    y += 7

    # --- consensus and finality
    panels.append(row("Consensus and finality", y)); y += 1
    panels.append(timeseries("Heights: head, preconf, safe, finalized", [
        q("block_latest_height", 'status="head"', "head (built)"),
        q("block_latest_height", 'status="preconf"', "preconf"),
        q("block_latest_height", 'status="safe"', "safe"),
        q("block_latest_height", 'status="finalized"', "finalized"),
    ], grid(0, y, 8, 8), desc="What the engine was last told. Safe and finalized follow OL epochs.", fill=0, decimals=0))
    panels.append(timeseries("Finality lag", [
        target(f'{P}block_latest_height{{instance=~"$instance",status="head"}} - ignoring(status) {P}block_latest_height{{instance=~"$instance",status="finalized"}}', "head − finalized"),
        target(f'{P}block_latest_height{{instance=~"$instance",status="head"}} - ignoring(status) {P}block_latest_height{{instance=~"$instance",status="safe"}}', "head − safe"),
    ], grid(8, y, 8, 8), desc="Blocks between the built head and the finalized / safe head.", decimals=0))
    panels.append(timeseries("OL epochs", [
        q("ol_epoch", 'status="latest"', "latest (OL)"),
        q("ol_epoch", 'status="local"', "local (applied to EE)"),
        q("ol_epoch", 'status="confirmed"', "confirmed"),
        q("ol_epoch", 'status="finalized"', "finalized"),
    ], grid(16, y, 8, 8), desc="Epochs as the OL tracker sees them, and the last one applied to EE state.", fill=0, decimals=0))
    y += 8
    panels.append(timeseries("Fork-choice updates", [
        rate_q("forkchoice_updates_total", "{{kind}} /min", mul=60, by="kind"),
        rate_q("forkchoice_failures_total", "failed /min", mul=60),
    ], grid(0, y, 8, 7), desc="Engine API forkchoiceUpdated calls per minute, by what drove them.", decimals=1))
    panels.append(timeseries("OL tracker actions", [
        rate_q("ol_tracker_actions_total", "{{action}} /min", mul=60, by="action"),
        rate_q("ol_tracker_failures_total", "failures /min", mul=60),
        rate_q("ol_epochs_applied_total", "epochs applied /min", mul=60),
    ], grid(8, y, 8, 7), desc="What each OL poll led to, per minute.", decimals=1))
    panels.append(timeseries("Gossip", [
        q("gossip_peers", legend="peers"),
        rate_q("gossip_broadcasts_total", "headers broadcast /min", mul=60),
        rate_q("gossip_received_total", "packages received /min", mul=60),
    ], grid(16, y, 8, 7), desc="The RLPx alpen_gossip subprotocol: peers connected, headers signed and broadcast (sequencer), packages received (full node).", decimals=1, fill=0))
    y += 7

    # --- DA on Bitcoin
    panels.append(row("Data availability on Bitcoin", y)); y += 1
    panels.append(timeseries("Envelopes submitted and finalized", [
        rate_q("da_envelopes_submitted_total", "submitted /min", mul=60),
        rate_q("batch_transitions_total", "DA complete /min", 'status="da_complete"', mul=60),
        rate_q("da_blobs_built_total", "blobs built /min", mul=60),
    ], grid(0, y, 8, 8), desc="Per minute. Each envelope is one commit and one reveal transaction.", decimals=1))
    panels.append(timeseries("Time to DA finality", [
        quant("batch_status_seconds", "0.5", "seal → DA finalized p50", 'status="da_complete"'),
        quant("batch_status_seconds", "0.9", "seal → DA finalized p90", 'status="da_complete"'),
    ], grid(8, y, 8, 8), unit="s", desc="From the batch being sealed to its envelope reaching the configured L1 confirmations.", decimals=1))
    panels.append(timeseries("Envelope status checks", [
        rate_q("da_status_checks_total", "{{status}} /min", mul=60, by="status"),
    ], grid(16, y, 8, 8), desc="What the envelope watcher found on each L1 poll, per minute.", decimals=1, stack=True))
    y += 8
    panels.append(timeseries("Blob size", [
        quant("da_blob_bytes", "0.5", "p50"),
        quant("da_blob_bytes", "0.99", "p99"),
        avg_q("da_blob_bytes", "mean"),
    ], grid(0, y, 8, 7), unit="bytes", desc="Bytes per DA blob (state diffs, bytecodes deduplicated)."))
    panels.append(timeseries("Blob composition", [
        quant("da_blob_blocks", "0.5", "blocks per blob p50"),
        quant("da_envelope_chunks", "0.5", "chunks per envelope p50"),
        rate_q("da_bytecodes_deduped_total", "bytecodes deduped /min", mul=60),
    ], grid(8, y, 8, 7), desc="", decimals=1, fill=0))
    panels.append(timeseries("DA bytes per minute", [
        target(f"rate({P}da_blob_bytes_sum{I}{RATE}) * 60", "blob bytes /min"),
    ], grid(16, y, 8, 7), unit="bytes", desc="Throughput written to Bitcoin."))
    y += 7

    # --- proving
    panels.append(row("Proving", y)); y += 1
    panels.append(timeseries("Proof tasks and receipts", [
        rate_q("proof_tasks_submitted_total", "{{kind}} tasks submitted /min", mul=60, by="kind"),
        rate_q("proof_receipts_total", "{{kind}} receipts /min", mul=60, by="kind"),
    ], grid(0, y, 8, 8), desc="Chunk proofs cover a chunk's blocks; the account proof covers a batch and waits for its chunks and DA.", decimals=1))
    panels.append(timeseries("Account proof wait", [
        quant("proof_wait_seconds", "0.5", "p50"),
        quant("proof_wait_seconds", "0.9", "p90"),
        quant("proof_wait_seconds", "1", "max"),
    ], grid(8, y, 8, 8), unit="s", desc="From a batch's proof tasks being submitted to its account proof being in the store. Includes waiting for the prover's next tick.", decimals=1))
    panels.append(timeseries("Proof status checks", [
        rate_q("proof_status_checks_total", "{{status}} /min", mul=60, by="status"),
    ], grid(16, y, 8, 8), desc="What the lifecycle found when it asked the prover about a batch, per minute.", decimals=1, stack=True))
    y += 8

    # --- OL submission
    panels.append(row("Submission to the OL", y)); y += 1
    panels.append(timeseries("Updates submitted", [
        rate_q("ol_updates_submitted_total", "first submissions /min", 'kind="first"', mul=60),
        rate_q("ol_updates_submitted_total", "retries /min", 'kind="retry"', mul=60),
        rate_q("ol_update_failures_total", "failed /min", mul=60),
    ], grid(0, y, 8, 7), desc="Snark account updates per minute. Resubmissions are updates sent again because the OL had not accepted them yet.", decimals=1))
    panels.append(timeseries("Submission lag", [
        target(f'{P}batch_latest_idx{{instance=~"$instance",status="sealed"}} - ignoring(status) {P}batch_latest_idx{{instance=~"$instance",status="ol_latest"}}', "sealed − OL latest (batches)"),
        target(f'{P}batch_latest_idx{{instance=~"$instance",status="proof_ready"}} - ignoring(status) {P}batch_latest_idx{{instance=~"$instance",status="ol_latest"}}', "proof-ready − OL latest"),
    ], grid(8, y, 8, 7), desc="Batches sealed but not yet submitted to the OL.", decimals=0))
    panels.append(timeseries("Update outputs and account sequence", [
        rate_q("ol_update_outputs_total", "{{kind}} outputs /min", mul=60, by="kind"),
        q("batch_latest_idx", 'status="ol_latest"', "OL latest (next seq no)"),
    ], grid(16, y, 8, 7), desc="Transfers and messages carried by updates; the OL account's next sequence number as last read.", decimals=1, fill=0,
       overrides=[{"matcher": {"id": "byName", "options": "OL latest (next seq no)"},
                   "properties": [{"id": "custom.axisPlacement", "value": "right"}]}]))
    y += 7

    # --- persistence
    panels.append(row("Persistence (MDBX store)", y)); y += 1
    panels.append(timeseries("Store size by environment", [
        q("store_env_bytes", legend="{{env}}"),
    ], grid(0, y, 8, 8), unit="bytes", desc="Bytes each MDBX environment's data file occupies on disk (sampled every 30 s).", stack=True))
    panels.append(timeseries("Rows by table", [
        q("store_rows", legend="{{env}}/{{table}}"),
    ], grid(8, y, 8, 8), desc="Entries per table, sampled every 30 s.", fill=0, decimals=0))
    panels.append(table("Store tables now", [q("store_rows", legend="{{env}}/{{table}}", instant=True)],
                        grid(16, y, 8, 8), desc="Rows per table at the last sample.",
                        transformations=[
                            {"id": "labelsToFields", "options": {"keepLabels": ["env", "table"], "mode": "columns"}},
                            {"id": "organize", "options": {"excludeByName": {"Time": True, "instance": True, "job": True, "role": True, "__name__": True},
                                                           "renameByName": {"Value": "rows"}, "indexByName": {"env": 0, "table": 1, "rows": 2}}},
                            {"id": "sortBy", "options": {"sort": [{"field": "rows", "desc": True}]}},
                        ]))
    y += 8

    # --- logs (needs the logs profile)
    panels.append(row("Logs (compose --profile logs: Loki + Alloy)", y, collapsed=True, panels=[
        logs("Warnings and errors", '{service="alpen-client"} |~ "WARN|ERROR"', grid(0, y + 1, 12, 10),
             desc="Container stdout shipped by Alloy. Nothing here until the logs profile runs."),
        logs("Batch lifecycle", '{service="alpen-client"} |~ "batch_lifecycle|update_submitter|envelope"', grid(12, y + 1, 12, 10),
             desc="Lines from the settlement tasks."),
    ]))
    y += 1

    return {
        "uid": "alpen-ee-service-map",
        "title": "Alpen EE Service Map",
        "description": "alpen-client's services, data flows and pipeline metrics. Generated by docker/observability/grafana/build_dashboard.py.",
        "tags": ["alpen", "ee"],
        "timezone": "browser",
        "editable": True,
        "graphTooltip": 1,
        "refresh": "10s",
        "time": {"from": "now-30m", "to": "now"},
        "timepicker": {"refresh_intervals": ["5s", "10s", "30s", "1m", "5m"]},
        "schemaVersion": 41,
        "version": 1,
        "templating": {"list": [{
            "name": "instance", "label": "node", "type": "query", "datasource": DS,
            "query": {"query": 'label_values(up{job="alpen-client"}, instance)', "refId": "var"},
            "definition": 'label_values(up{job="alpen-client"}, instance)',
            "refresh": 2, "includeAll": False, "multi": False, "sort": 1,
            "current": {}, "options": [],
        }]},
        "annotations": {"list": [{
            "builtIn": 1, "datasource": {"type": "grafana", "uid": "-- Grafana --"},
            "enable": True, "hide": True, "iconColor": "rgba(0, 211, 255, 1)",
            "name": "Annotations & Alerts", "type": "dashboard",
        }]},
        "panels": panels,
    }


# ------------------------------------------------------------ fleet form ---

COMPOSE_SELECTOR = 'instance=~"$instance"'
CLUSTER_SELECTOR = 'namespace="$rollup_namespace", pod=~"$pod"'
COMPOSE_LOG_STREAM = '{service="alpen-client"}'
CLUSTER_LOG_STREAM = '{k8s_namespace_name="$rollup_namespace", k8s_pod_name=~"$pod"}'
# Labels the PodMonitor adds to every series; dropped from the table views.
CLUSTER_TABLE_LABELS = ("namespace", "pod", "container", "endpoint", "job", "instance")


def _rewrite_strings(node, fn):
    if isinstance(node, dict):
        return {k: _rewrite_strings(v, fn) for k, v in node.items()}
    if isinstance(node, list):
        return [_rewrite_strings(v, fn) for v in node]
    if isinstance(node, str):
        return fn(node)
    return node


def _cluster_text(text):
    return (text.replace(COMPOSE_SELECTOR, CLUSTER_SELECTOR)
                .replace(COMPOSE_LOG_STREAM, CLUSTER_LOG_STREAM)
                .replace("Container stdout shipped by Alloy. Nothing here until the logs profile runs.",
                         "Container stdout, via the OTel collector and Loki.")
                .replace("Logs (compose --profile logs: Loki + Alloy)", "Logs"))


def to_cluster(dash, namespace):
    """The compose dashboard re-targeted at a kube-prometheus-stack Grafana."""
    dash = _rewrite_strings(dash, _cluster_text)
    prom = {"type": "prometheus", "uid": "${prometheus_datasource}"}
    loki = {"type": "loki", "uid": "${loki_datasource}"}

    def retarget(node):
        if isinstance(node, dict):
            if node.get("uid") == DS["uid"]:
                return prom
            if node.get("uid") == LOKI["uid"]:
                return loki
            if node.get("id") == "organize":
                exclude = node["options"].setdefault("excludeByName", {})
                exclude.update({label: True for label in CLUSTER_TABLE_LABELS})
            return {k: retarget(v) for k, v in node.items()}
        if isinstance(node, list):
            return [retarget(v) for v in node]
        return node

    dash = retarget(dash)
    dash["refresh"] = "30s"  # the PodMonitor scrapes every 30 s
    dash["time"] = {"from": "now-1h", "to": "now"}
    dash["description"] = (
        "alpen-client's services, data flows and pipeline metrics. Generated by "
        "alpen-ee docker/observability/grafana/build_dashboard.py --target cluster; edit there."
    )
    pod_query = f'label_values({P}block_latest_height{{namespace="$rollup_namespace",status="head"}}, pod)'
    dash["templating"] = {"list": [
        {"name": "prometheus_datasource", "label": "Prometheus", "type": "datasource", "query": "prometheus",
         "refresh": 1, "hide": 0, "current": {}, "options": [], "regex": ""},
        {"name": "loki_datasource", "label": "Loki", "type": "datasource", "query": "loki",
         "refresh": 1, "hide": 0, "current": {}, "options": [], "regex": ""},
        {"name": "rollup_namespace", "type": "constant", "query": namespace, "hide": 2,
         "current": {"text": namespace, "value": namespace}, "options": []},
        {"name": "pod", "label": "node", "type": "query", "datasource": prom,
         "query": {"query": pod_query, "refId": "var"}, "definition": pod_query,
         "refresh": 2, "includeAll": False, "multi": False, "sort": 1, "current": {}, "options": []},
    ]}
    return dash


def configmap(dash, folder):
    """The dashboard as a ConfigMap for the Grafana sidecar."""
    body = json.dumps(dash, indent=2)
    indented = "\n".join("    " + line if line else "" for line in body.splitlines())
    return (
        "# Generated by alpen-ee docker/observability/grafana/build_dashboard.py --target cluster.\n"
        "# Edit the generator and regenerate; do not hand-edit the JSON.\n"
        "apiVersion: v1\n"
        "kind: ConfigMap\n"
        "metadata:\n"
        f'  name: "grafana-dashboard-{dash["uid"]}"\n'
        '  namespace: "monitoring"\n'
        "  labels:\n"
        '    grafana_dashboard: "1"\n'
        "  annotations:\n"
        f'    grafana_folder: "{folder}"\n'
        "data:\n"
        f'  {dash["uid"]}.json: |-\n'
        f"{indented}\n"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", choices=["compose", "cluster"], default="compose")
    ap.add_argument("--namespace", default="rollup", help="rollup namespace in the cluster (cluster target)")
    ap.add_argument("--folder", default="Testnet", help="Grafana folder annotation (cluster target)")
    ap.add_argument("--out", type=Path, help="output path (default: dashboards/<uid>.json or stdout for cluster)")
    args = ap.parse_args()

    dash = build()
    if args.target == "compose":
        out = args.out or OUT
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(dash, indent=2) + "\n")
        print(f"wrote {out}")
        return
    text = configmap(to_cluster(dash, args.namespace), args.folder)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text)
        print(f"wrote {args.out}")
    else:
        print(text, end="")


if __name__ == "__main__":
    main()
