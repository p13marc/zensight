# Sensors & Alerting: `nlink` + `netring` → ZenSight

**Status:** research / proposal for review
**Date:** 2026-06-15
**Author:** Claude (research pass)
**Supersedes:** the earlier "Bridge Ideas" draft (same content, reframed around
alerting + expected-state assertions, plus a crate-renaming proposal).
**Implementation plans:** detailed, source-verified plans derived from this report
live in [`docs/plans/`](plans/00-INDEX.md) (start at `00-INDEX.md`).

This report covers three things:

1. **A renaming proposal** — "bridge" is the wrong word for these crates
   (§1).
2. **Two new sensors** built on your own pure-Rust Linux networking libraries
   ([`nlink`](https://github.com/p13marc/nlink),
   [`netring`](https://github.com/p13marc/netring)) (§2).
3. **The headline use case you asked for:** emit alerts on (a) **network
   anomalies** and (b) **deviation from expected machine state** — "this socket
   *should* be listening", "this connection *should* be established", "this
   interface *should* be up", "this peer *should* be handshaking" (§3–§5).

Both libraries are pure Rust, edition 2024, tokio-native, no native C deps — they
drop straight into the ZenSight workspace and the existing sensor framework
(publisher + health reporter + liveness + correlation) with no friction.

---

## 1. Renaming: stop calling them "bridges"

"Bridge" is wrong for two reasons:

1. **It collides with Zenoh's own vocabulary.** In the upstream Zenoh world a
   *bridge* is a specific thing (e.g. DDS/MQTT bridges that splice another pub/sub
   bus onto Zenoh). Our crates don't splice a bus — they *observe a system and
   publish telemetry*. The name invites the wrong mental model.
2. **It undersells what they now do.** With the alerting work below, these crates
   don't just forward bytes — they **sense** state, **assert** expectations, and
   **emit alerts**. "Bridge" describes a pipe; these are sensors with opinions.

### Proposed scheme

Unify everything under the `zensight-` prefix and split by role:

| Today | Proposed | Role |
|---|---|---|
| `zenoh-bridge-snmp` | `zensight-sensor-snmp` | telemetry source |
| `zenoh-bridge-syslog` | `zensight-sensor-syslog` | telemetry source |
| `zenoh-bridge-netflow` | `zensight-sensor-netflow` | telemetry source |
| `zenoh-bridge-modbus` | `zensight-sensor-modbus` | telemetry source |
| `zenoh-bridge-sysinfo` | `zensight-sensor-sysinfo` | telemetry source |
| `zenoh-bridge-gnmi` | `zensight-sensor-gnmi` | telemetry source |
| *(new)* | `zensight-sensor-netlink` | telemetry source (nlink) |
| *(new)* | `zensight-sensor-netring` | telemetry source (netring) |
| `zensight-bridge-framework` | `zensight-sensor-core` | shared sensor framework |
| *(new)* | `zensight-sentinel` | **expectation/alert engine** (see §3) |

> **Why "sensor" + "sentinel".** A *sensor* observes and reports (telemetry). A
> *sentinel* stands watch and raises the alarm when something is wrong
> (anomalies + broken expectations). The two words cleanly separate "here is what
> I see" from "here is what is wrong", which is exactly the split you're asking
> for. If you prefer a single word, `zensight-probe-*` or `zensight-watch-*` also
> work — but I'd keep sensor (passive, always-on) distinct from sentinel
> (evaluative, alert-emitting).

**Naming candidates considered:** `agent` (overloaded, implies remote install),
`collector` (we're the collector already — the frontend), `monitor` (collides
with netring's `Monitor` type and is vague), `exporter` (already used for the
Prometheus/OTEL *outbound* crates — keep that meaning). `sensor` won.

The rename is mechanical (crate dirs + `Cargo.toml` names + workspace members +
docs); the `Protocol` enum, key expressions, and `TelemetryPoint` model are
unchanged. Suggest doing it as one `chore: rename bridges to sensors` commit
before building the new crates, so the new crates are born with the right name.

---

## 2. The two new sensors (telemetry sources)

These remain valuable as plain telemetry sources, and they're also the *data
substrate* the sentinel evaluates expectations against. Brief recap; the
alerting design in §3–§5 is the new heart of the report.

### 2.1 `zensight-sensor-netlink` (built on `nlink`)

Kernel ground truth, no SNMP, with **push events** (not just polling):

- **Interfaces:** per-iface + per-queue RX/TX bytes/packets/errors/drops, MTU,
  oper/carrier state, ethtool `-S` stats and ring drops (data SNMP can't see).
- **Events (push):** link up/down, address add/del, route changes, neighbor
  (ARP/NDP) state transitions — millisecond latency vs. SNMP poll cycles.
- **Sockets (`sockdiag`):** TCP state, RTT, cwnd, retransmits, recv/send-Q,
  listen sockets — the substrate for "expected socket" assertions (§4).
- **TC / QoS:** per-qdisc/class backlog, drops, overlimits.
- **Tunnels:** WireGuard per-peer last-handshake age + bytes; XFRM/IPsec SA
  health.
- **Diagnostics:** nlink ships a `Diagnostics::scan()` that already returns a
  severity-scored `DiagnosticReport` + `find_bottleneck()` with a `0.0..=1.0`
  score and a recommendation string — a ready-made health-assertion engine we
  can surface directly (§4.3).

Key expr: `zensight/netlink/<host>/iface/<name>/<stat>`,
`.../sockets/tcp/<state>`, `.../tc/<iface>/<qdisc>/drops`,
`.../wireguard/<peer>/last_handshake_age`.

### 2.2 `zensight-sensor-netring` (built on `netring`)

Wire ground truth from zero-copy capture (AF_PACKET / AF_XDP), no NetFlow
exporter device required:

- **Flows:** 5-tuple records, bytes/packets per direction, duration, TCP flag
  history, per-app bandwidth (`on_bandwidth` + `LabelTable`).
- **L7:** DNS (queries/responses/RTT/**unanswered**), HTTP (status/method/host),
  TLS/JA3/JA4/SNI/ECH.
- **Anomaly detectors (prebuilt):** port scan (TRW), beacon (period variance,
  C2), DGA (DNS bigram entropy), ECH downgrade — the substrate for network-anomaly
  alerts (§3.1).
- **Capture health:** `netring`'s own `METRICS.md` defines windowed
  `capture_drop_rate`, active-flow gauge, handler/backend error counters — feed
  these to the sensor's own `HealthReporter` so the dashboard knows when the
  sensor itself is lossy.

netring already emits IPFIX/syslog/OTLP/EVE/Prometheus; the clean integration is
a small **`ZensightSink`** implementing netring's sink trait to publish
`TelemetryPoint`s and alerts straight onto Zenoh (Strategy A — recommended over
chaining through the existing netflow/syslog sensors).

---

## 3. `zensight-sentinel` — the alert engine

This is the new centerpiece. The sentinel turns the two sensors from "here is
data" into "here is what is **wrong**." It produces two classes of alert from one
uniform model:

- **Pillar A — Network anomalies** (something bad is *happening*): port scan, C2
  beacon, DGA domain, TLS downgrade, packet-loss spike, route flap.
- **Pillar B — Expectation violations** (something expected is *not true*): a
  socket that should be listening isn't; a connection that should be established
  is down; an interface that should be up is down; a WireGuard peer that should
  be handshaking has gone stale; a default route that should exist is missing.

Both pillars share one **`Alert`** shape and one publish channel, so the existing
**Alerts** view (`view/alerts.rs`, `AlertsState`) and **toast** system render them
uniformly.

### 3.1 Pillar A — network anomaly alerts (netring)

Wire netring's `Monitor` detectors and translate each emitted anomaly into an
`Alert`. netring's detectors already carry a `kind` slug and a severity, and the
detector guide (`WRITING_DETECTORS.md`) shows how to add custom ones via
`ctx.emit(kind, severity)`. Map:

| netring detector | Alert | Default severity |
|---|---|---|
| `PortScanDetector` (TRW) | "Port scan from `<ip>` (`<n>` ports)" | warning/critical by score |
| `BeaconDetector` | "Periodic beacon `<src>`→`<dst>` (C2-like)" | warning |
| `DgaScorer` | "DGA-like DNS query `<domain>`" | warning |
| ECH/TLS downgrade | "TLS downgrade / ECH anomaly `<sni>`" | info/warning |
| `capture_drop_rate` over threshold | "Sensor dropping packets (`<rate>%`)" | critical (self-health) |

High-cardinality detail (IP, domain, JA4) goes in the alert **labels/body**, never
in a metric series name — following netring's own cardinality guidance.

### 3.2 Pillar B — expected-machine-state assertions (nlink + netring)

The model you asked for: **declare expected state, diff against observed state,
alert on the delta.** This is exactly what nlink's declarative layer already does
for *configuration* — `NetworkConfig::new()...diff(&conn)` computes a structured
diff of desired-vs-kernel — except the sentinel runs the diff **read-only** and
emits an alert per deviation instead of applying changes.

The same shape generalizes beyond config to *runtime* expectations (sockets,
connections, neighbor reachability, tunnel liveness, DNS resolvability). See §4
for the catalog and §5 for the config schema.

### 3.3 Alert shape & channels

One `Alert` type (proposed, lives in `zensight-common`):

```rust
pub struct Alert {
    pub timestamp: i64,
    pub source: String,             // host / sensor id
    pub kind: AlertKind,            // Anomaly | ExpectationViolation
    pub rule: String,               // e.g. "ssh-listening" or "PortScanDetector"
    pub severity: Severity,         // Info | Warning | Critical  (reuse syslog severity)
    pub summary: String,            // human text for toast/alert row
    pub labels: HashMap<String, String>, // ip, port, peer, sni, expected, actual...
    pub state: AlertState,          // Firing | Resolved   (so the UI can auto-clear)
}
```

Published on a dedicated channel that the frontend already conceptually has room
for:

```
zensight/<sensor>/@/alerts            # firing + resolved alerts
zensight/_meta/expectations/<host>/*  # which expectations are configured (for the UI)
```

> **Resolved alerts matter.** Because expectations are evaluated continuously, the
> sentinel knows when a violation *clears* (socket came back, peer handshook). Emit
> `state: Resolved` so the Alerts view auto-closes the row and toasts a recovery —
> a big UX win over fire-only alerting.

---

## 4. Expectation catalog (Pillar B)

What you can assert with nlink/netring today, mapped to the underlying API.

### 4.1 Socket / connection expectations (nlink `sockdiag`) — your headline case

nlink's `SocketFilter` is purpose-built for this. From the `sockdiag` examples:

```rust
// "port 22 must be LISTENing"
SocketFilter::tcp().listening().with_tcp_info().build()
// "this app must hold an ESTABLISHED connection to the DB"
SocketFilter::tcp().states(&[TcpState::Established]).build()
```

Assertions we can offer:

| Expectation | How evaluated | Example alert |
|---|---|---|
| Port `<p>` is **listening** | `SocketFilter::tcp().listening()` contains `<p>` | "sshd not listening on :22" |
| **N** established conns to `<peer:port>` | filter `Established` + match remote | "0/1 expected DB connections to 10.0.0.5:5432" |
| No listener on a **forbidden** port | listening set excludes `<p>` | "unexpected listener on :23 (telnet)" |
| Connection health within bounds | `tcp_info.rtt`, `retransmits`, `recv_q` thresholds | "DB conn RTT 480ms > 200ms; 1.2k retransmits" |
| Listen backlog not overflowing | recv-Q on listen socket | "listen backlog saturating on :443" |

This directly answers "alert when the expected machine state is not right —
expected socket to be open/listen with connection established."

### 4.2 Link / address / route / neighbor expectations (nlink RTNetlink)

Reuse the `NetworkConfig::diff` model read-only, *or* hand-roll simple checks
against `get_links()` / route / neighbor dumps:

| Expectation | Example alert |
|---|---|
| Interface `<name>` is **up / carrier present** | "eth1 down (expected up)" |
| Interface has expected IP/CIDR | "eth0 missing 10.0.0.1/24" |
| **Default route** present via expected gateway | "default route missing / gw changed" |
| Specific route present (e.g. to a partner subnet) | "route to 10.9.0.0/16 withdrawn" |
| Neighbor `<ip>` REACHABLE (gateway/peer up) | "gateway 10.0.0.1 FAILED (ARP)" |
| MTU matches expected (path-MTU / jumbo) | "eth0 MTU 1500 (expected 9000)" |

These also drive the topology view: an expected-but-absent edge is both an alert
*and* a visibly broken link on the graph.

### 4.3 Diagnostics-derived expectations (nlink `Diagnostics`)

nlink ships `Diagnostics::scan()` → `DiagnosticReport` with `Severity`-tagged
issues, and `find_bottleneck()` → a `0.0..=1.0` score (drop-rate ×0.6 + backlog
×0.3 + error ×0.1) plus a `recommendation` string. The sentinel can run the scan
on a cadence and emit an alert for every issue above a configured severity — no
hand-written rules needed for the common cases. "Worst bottleneck score > 0.7"
is a single, tunable alert that covers a whole class of problems.

### 4.4 Tunnel / VPN expectations (nlink WireGuard, XFRM)

| Expectation | Example alert |
|---|---|
| WireGuard peer last-handshake age < `<t>` | "wg peer `gw2` no handshake for 5m" |
| Peer tx/rx advancing (not stalled) | "wg peer `gw2` tunnel idle / stalled" |
| IPsec SA present & not near expiry / no replay errors | "IPsec SA to 10.9.0.1 expiring; replay errors rising" |

### 4.5 Service-reachability expectations (netring L7)

Passive, no synthetic probe needed:

| Expectation | How | Example alert |
|---|---|---|
| DNS resolver answering | netring DNS parser unanswered-query ratio | "DNS server 10.0.0.53: 40% queries unanswered" |
| HTTP endpoint healthy | netring HTTP 5xx rate per host | "api.internal 5xx rate spiking" |
| Expected client/asset present | netring JA4/asset inventory absence | "expected TLS client `backup-agent` not seen in 1h" |

---

## 5. Configuring expectations (JSON5)

Expectations live in the sensor/sentinel config, consistent with the existing
JSON5 convention in `configs/`. Sketch:

```json5
{
  zenoh: { mode: "peer", connect: ["tcp/localhost:7447"] },

  // Pillar B — declared expectations for THIS host.
  expectations: {
    sockets: [
      { name: "sshd",   listen: "tcp/0.0.0.0:22",  severity: "critical" },
      { name: "db-conn", established_to: "10.0.0.5:5432", min: 1, severity: "warning",
        rtt_ms_max: 200, retransmits_max: 500 },
      { name: "no-telnet", forbid_listen: "tcp/*:23", severity: "critical" },
    ],
    links: [
      { iface: "eth0", up: true, addr: "10.0.0.1/24", mtu: 9000, severity: "critical" },
    ],
    routes: [
      { default_via: "10.0.0.254", severity: "critical" },
      { to: "10.9.0.0/16", severity: "warning" },
    ],
    neighbors: [
      { ip: "10.0.0.254", reachable: true, severity: "warning" },  // gateway ARP
    ],
    wireguard: [
      { peer: "gw2", handshake_max_age_s: 180, severity: "warning" },
    ],
    diagnostics: { max_bottleneck_score: 0.7, min_issue_severity: "warning" },
  },

  // Pillar A — anomaly detectors to run (netring).
  anomalies: {
    interfaces: ["eth0"],
    detectors: ["port_scan", "beacon", "dga", "ech_downgrade"],
    capture_drop_rate_alert: 0.01,   // self-health
  },
}
```

The sentinel loads this, evaluates on a cadence (and on push events where nlink
provides them), and publishes `Alert`s to `zensight/<sensor>/@/alerts`. Defaults
should be conservative; everything is severity-tunable.

---

## 6. Recommendation & sequencing

| # | Work | Library | Effort | Why now |
|---|------|---------|--------|---------|
| 0 | **Rename** `zenoh-bridge-*` → `zensight-sensor-*`, framework → `zensight-sensor-core` | — | **S** | Mechanical; do it before new crates so they're born named right. |
| 1 | `zensight-common`: add `Alert`/`AlertKind`/`Severity`/`AlertState` + `@/alerts` channel; Alerts view consumes it | — | **S** | Shared substrate for both pillars; small, unblocks everything. |
| 2 | `zensight-sensor-netlink` (interfaces + events + sockdiag) | nlink | **M** | Flagship telemetry + the substrate for socket/link expectations. |
| 3 | `zensight-sentinel` Pillar B: socket + link + route + neighbor expectations via nlink `diff`/`sockdiag` | nlink | **M** | **Your headline ask.** Reuses nlink's diff/sockdiag — little new logic. |
| 4 | `zensight-sensor-netring` + `ZensightSink` | netring | **M** | Real flows; substrate for anomaly alerts. |
| 5 | `zensight-sentinel` Pillar A: wire netring detectors → `Alert` | netring | **S** | Detectors prebuilt; opens the network-anomaly alert category. |
| 6 | Diagnostics-scan alerts + WireGuard/L7 expectations | both | **S each** | Incremental, high value-per-line. |
| 7 | Topology: expected-but-absent edges shown as broken + flow-sized edges | both | **M** | High visual impact; depends on 2 & 4. |

**Design notes / risks:**
- **Sentinel placement.** Evaluate expectations *locally in the sensor* where it
  has raw nlink/netring state (lowest latency, can assert on data it never
  publishes), but define the `Alert` type and expectation schema centrally in
  `zensight-common` so alerts are uniform. `zensight-sentinel` can be a library
  the sensors embed, or a standalone binary that also subscribes to Zenoh
  telemetry for cross-host expectations — start embedded, extract later.
- **Read-only diff.** Use nlink's `diff` for *evaluation only*; never call
  `apply`. Document that the sensor needs no `CAP_NET_ADMIN` for assertions
  (read dumps work unprivileged; sockdiag/Diagnostics scan unprivileged per the
  examples). netring capture needs `CAP_NET_RAW` (+`CAP_IPC_LOCK` for AF_XDP).
- **Toolchain:** both crates need Rust 1.95+ / edition 2024 (ZenSight is already
  2024 — confirm the pinned toolchain). netring AF_XDP needs kernel 5.4+; default
  to AF_PACKET (kernel 3.2+).
- **Flap control.** Expectation evaluation must debounce (the framework already
  has a rolling error window + liveness) so a 1-second blip doesn't toast. Add a
  `for: <duration>` ("must be violated continuously for N s") option, Prometheus-
  alert style.

---

## 7. One-paragraph pitch

> Rename the `zenoh-bridge-*` crates to `zensight-sensor-*` — "bridge" collides
> with Zenoh's own meaning and undersells them — and add two new sensors plus a
> `zensight-sentinel` alert engine. The netlink sensor (built on your `nlink`)
> streams kernel ground truth and feeds a declarative expectation engine: declare
> that a socket must be listening, a connection established, an interface up, a
> route present, a WireGuard peer handshaking — the sentinel runs nlink's diff
> read-only and alerts on every deviation, then auto-resolves when it heals. The
> netring sensor (built on your `netring`) streams real flows and feeds ready-made
> anomaly detectors — port scan, C2 beacon, DGA — giving ZenSight its first
> network-threat alerts. Two pillars, one `Alert` type, straight into the existing
> Alerts and toast UI, all from your own pure-Rust, edition-2024 libraries with
> almost no new abstractions.
