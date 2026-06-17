# ZenSight v2 — Greatly Improve the netlink & netring Sensors (+ GUI)

**Status:** proposal for review
**Date:** 2026-06-17
**Builds on:** the implemented sensors+alerting redesign (`docs/plans/00-INDEX.md`,
Plans 01–11, merged to master). This series *extends* what those shipped.

The v1 sensors deliberately shipped a thin slice: netlink does interface counters
+ socket aggregates + a few expectations; netring does flow counts + per-app
bandwidth + a port-scan detector. Both `nlink` and `netring` expose **far** more,
and the GUI shows only a fraction. This series turns them into rich, dynamically
configurable, alert-heavy observability + security sensors.

## The plans

| # | Plan | Theme |
|---|------|-------|
| 01 | [netlink sensor v2](01-netlink-sensor.md) | many more metrics (routes, neighbors, QoS/TC, ethtool, WireGuard, per-socket health, diagnostics, push events), more expectations, dynamic config |
| 02 | [netlink GUI v2](02-netlink-gui.md) | tabbed host view, charts, drill-down, expectation status inline, topology neighbor-edge enrichment |
| 03 | [netring sensor v2](03-netring-sensor.md) | real flow records (IPFIX fields), L7 (DNS/HTTP/TLS+JA4), capture health, ICMP/RST, many detectors (beacon/DGA/exfil/lateral/ECH), dynamic config (BPF/detector tuning) |
| 04 | [netring GUI v2](04-netring-gui.md) | flow explorer, top-talkers, L7 panels, JA4 asset inventory, security drill-down |

---

## 1. Cross-cutting design principles

These govern every plan below — read first.

### P1 — Detect at the edge; manage centrally
Per the architecture analysis (`what's-next` discussion): the sensor evaluates
conditions that need **local/raw/high-volume state** (socket tables, raw packets)
and emits `common::Alert`. New detectors/expectations stay **in-sensor**. Don't
ship the firehose to a central evaluator. Cross-host correlation is the only
detection that belongs in a future central consumer (out of scope here).

### P2 — Aggregate by default, detail on demand (the cardinality rule)
This is the single most important rule for v2. Per-flow, per-socket, per-route,
per-neighbor data is **high-cardinality** — streaming it all as telemetry would
blow up the bus, the exporters' TSDB, and the GUI. So:

- **Stream** low-cardinality **aggregates** continuously (counts by state, top-N
  rollups, rates, percentiles).
- **Expose detail via a Zenoh queryable** (`@/query/<topic>`) the GUI calls
  **on demand** (open a host → fetch its full socket/route/neighbor table; open a
  flow row → fetch the conversation). Detail is pulled, never pushed.
- High-cardinality identifiers (IP, port, JA4, domain) live in **alert labels /
  query replies**, never in a streamed metric series name. (node_exporter +
  netring `METRICS.md` both stress this — see Sources.)

This introduces a new pattern: **per-sensor on-demand query channels** alongside
the existing telemetry/alert/command channels.

### P3 — Metric taxonomy: USE for interfaces, RED for flows/L7
- **Interfaces / links (USE method):** Utilization (bytes/sec), Saturation
  (queue backlog, drops, fifo), Errors (rx/tx errors, crc, frame, carrier).
- **Flows / L7 (RED method):** Rate (flows/sec, requests/sec), Errors (resets,
  5xx, unanswered DNS, ICMP unreachable), Duration (flow duration, RTT, L7
  latency).
This keeps the metric set principled, not a dump of every kernel counter.

### P4 — Everything dynamically configurable
Every collection toggle, detector, expectation, filter, and threshold is
changeable at runtime over the command channel (generalize Plan 08's
`@/commands/expectations` to `@/commands/<topic>`), with a status queryable
reflecting the live config. No restart to change what a sensor collects or
detects. The GUI is the primary author (Plan 08 foundation already exists).

### P5 — Alert lifecycle + auto-resolve
Every new alert family integrates with `AlertReporter` (firing/resolved,
debounce, reconcile). Fix the v1 gap: **anomaly alerts must auto-resolve** via a
TTL/quiet-period sweep, not linger forever.

---

## 2. What v1 already has (don't re-plan)

- netlink: `iface/<name>/{rx,tx}_{bytes,packets,errors,dropped}`, up/carrier/
  oper_state/mtu/info; `sockets/tcp/{established,listen,time_wait,syn_sent,
  close_wait}`, retransmits_total, max_rtt_us. Sentinel expectations: socket
  listen/established/forbid, link up. Command channel: `@/commands/expectations`.
- netring: `flow/{started,ended}_total`, `flow/active`, `bandwidth/<app>/
  bytes_per_sec`; `PortScanTRW` anomaly via `ChannelSink`. pcap replay support.
- GUI: specialized netlink (interfaces+sockets) and netring (flows+bandwidth)
  views; Security view (anomaly lens); topology alert overlay; Expectations
  authoring view.

---

## 3. Sequencing

Land sensor-side first (telemetry/alerts/query channels), then the GUI that
consumes them. Suggested order:

1. **01 §A (netlink metrics)** + **03 §A (netring flows/L7)** — the telemetry +
   query channels. Highest value, unblocks the GUI.
2. **01 §B/§C** + **03 §B/§C** — new alerts + dynamic config.
3. **02** + **04** — the GUI views that surface it all.
4. Topology neighbor-edge enrichment (02 §5) last — depends on 01 neighbor data.

Each plan is independently shippable and independently testable (pure mapping +
checks unit-tested; live verified via unprivileged netlink reads / netring pcap
replay, as in v1).

---

## 4. Sources (best-practice grounding)

- USE method + interface counters (errs/drop/fifo/carrier; rate()×8 for bits;
  cardinality filtering): [node_exporter network metrics](https://www.robustperception.io/network-interface-metrics-from-the-node-exporter/),
  [node_exporter repo](https://github.com/prometheus/node_exporter).
- Flow record fields (5-tuple, bytes/packets, TCP flags, start/end, duration;
  top-talkers/conversation analytics): [NetFlow→IPFIX evolution](https://www.noction.com/blog/network-flow-monitoring),
  [RFC 3954 NetFlow v9](https://datatracker.ietf.org/doc/html/rfc3954).
- NTA detections (beaconing/C2, DGA use-and-discard DNS, exfil volume anomalies,
  lateral-movement internal scanning): [Network Traffic Analysis guide](https://cyberdefenders.org/blog/the-ultimate-guide-to-network-traffic-analysis-for-soc-analysts/),
  [exfiltration detection](https://fidelissecurity.com/threatgeek/network-security/network-traffic-analysis-for-data-exfiltration-detection/),
  [C2 detection](https://hunt.io/glossary/detect-c2).
