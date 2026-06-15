# Plan 04 — `zensight-sentinel` (Pillar B: expected machine state)

**Goal:** the headline ask. Declare what the machine *should* look like — a socket
listening, a connection established, an interface up, a route present, a gateway
reachable, a WireGuard peer handshaking — evaluate it continuously against
`nlink`-observed state, and emit `Alert`s (firing/resolved) when reality
diverges.

**Depends on:** 02 (`Alert`, `AlertReporter`), 03 (the netlink sensor, where the
evaluator is embedded and gets its `Connection`s). **Effort:** M–L.

Model: this is nlink's declarative `NetworkConfig::diff()` idea run **read-only**
— "desired vs actual → delta" — generalized from config to runtime state.

---

## 1. Crate shape

`zensight-sentinel/` is a **library** (no binary yet — embedded in
`sensor-netlink`; standalone binary deferred per INDEX §5).

```
zensight-sentinel/
├── Cargo.toml          # deps: zensight-common, zensight-sensor-core, nlink, tokio, serde
├── src/
│   ├── lib.rs
│   ├── config.rs       # ExpectationsConfig (the JSON5 schema)
│   ├── evaluator.rs    # Evaluator: owns nlink Connections, runs sweeps, feeds AlertReporter
│   ├── expectation.rs  # the Expectation trait + Outcome
│   └── checks/         # one module per expectation family
│       ├── socket.rs   # listening / established / forbidden-listen / conn-health
│       ├── link.rs     # iface up/addr/mtu
│       ├── route.rs     # default route / specific route present
│       ├── neighbor.rs # gateway/peer reachable (ARP/NDP)
│       ├── wireguard.rs# handshake freshness / tunnel progressing
│       └── diagnostics.rs # nlink Diagnostics::scan threshold
```

## 2. The core trait (`expectation.rs`)

```rust
/// One declared expectation. Evaluated each sweep against fresh observed state.
#[async_trait::async_trait]
pub trait Expectation: Send + Sync {
    /// Stable rule slug, e.g. "socket:sshd". Used as `Alert.rule` + dedup key root.
    fn rule(&self) -> &str;
    fn severity(&self) -> AlertSeverity;
    /// Minimum continuous-violation duration before firing (per-rule `for:`).
    fn for_duration(&self) -> Option<Duration> { None }

    /// Evaluate. Return zero or more *currently-violated* facts. An empty vec
    /// means "satisfied" → the reporter resolves any prior alert for this rule.
    async fn evaluate(&self, ctx: &EvalCtx) -> Result<Vec<Violation>>;
}

pub struct Violation {
    /// Distinguishes multiple violations under one rule (e.g. per-peer).
    pub instance: String,           // "" if the rule is singular
    pub summary: String,
    pub labels: HashMap<String, String>,   // expected/actual/ip/port/...
    pub severity_override: Option<AlertSeverity>,
}
```

`EvalCtx` holds the shared `nlink` connections (`Connection<Route>`,
`Connection<SockDiag>`, a `Diagnostics`) + the host id, so checks don't each open
sockets. It also carries the *last event* (from Plan 03's event stream) so an
event-triggered sweep can short-circuit to the affected check.

## 3. The evaluator loop (`evaluator.rs`)

```rust
pub struct Evaluator {
    expectations: Vec<Box<dyn Expectation>>,
    reporter: AlertReporter,             // from sensor-core, protocol = Netlink
    ctx: EvalCtx,
    interval: Duration,
}

impl Evaluator {
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        loop {
            tick.tick().await;
            self.sweep().await;
        }
    }

    /// Optionally call from the netlink event stream for instant re-eval.
    pub async fn on_event(&self, ev: &NetworkEvent) { /* re-run affected checks */ }

    async fn sweep(&self) {
        for exp in &self.expectations {
            match exp.evaluate(&self.ctx).await {
                Ok(violations) => {
                    let keys: Vec<String> = violations.iter()
                        .map(|v| format!("{}:{}", exp.rule(), v.instance)).collect();
                    for v in &violations {
                        let alert = Alert::new(host, Protocol::Netlink, AlertKind::Expectation,
                            exp.rule(), v.severity_override.unwrap_or(exp.severity()), &v.summary)
                            .with_labels(v.labels.clone());
                        self.reporter.observe(alert, exp.for_duration()).await.ok();
                    }
                    // resolve any previously-firing instance of this rule now satisfied
                    self.reporter.reconcile(exp.rule(), &keys).await.ok();
                }
                Err(e) => { /* report eval error to @/errors; do NOT resolve (unknown state) */ }
            }
        }
    }
}
```

**Critical correctness rule:** on evaluation *error* (e.g. netlink query failed),
do **not** resolve existing alerts — we don't know the state. Only an explicit
"satisfied" (Ok with the instance absent) resolves.

## 4. The checks

### 4.1 Sockets (`checks/socket.rs`) — the headline case
```rust
// "port 22 must be LISTENing"
let socks = ctx.sockdiag.query(&SocketFilter::tcp().listening().build()).await?;
let listening = socks.iter().filter_map(inet_local_port).collect::<HashSet<_>>();
if !listening.contains(&self.port) {
    violations.push(Violation{ instance:"", summary: format!("{} not listening on :{}", self.name, self.port),
        labels: hashmap!{"expected"=>"listen", "port"=>self.port} });
}
```
- **established_to**: query `.states(&[Established]).with_tcp_info()`, count matches
  of `remote == peer:port`; violate if `count < min`. Optionally check `tcp_info.rtt`
  / `retransmits` / `recv_q` against bounds → separate violation instance
  `health`.
- **forbid_listen**: violate if the forbidden port *is* in `listening`.

### 4.2 Links (`checks/link.rs`)
`conn.get_links()`, match by name → check `is_up()`, expected addr present
(`conn.get_addresses()`), `mtu()`. Each mismatch a violation.

### 4.3 Routes (`checks/route.rs`)
Dump routes; check default route exists & `via` matches; check a specific
`to`-prefix is present.

### 4.4 Neighbors (`checks/neighbor.rs`)
Dump neighbors (`route/neighbors`); for the gateway IP check state is
`REACHABLE`/`STALE` (not `FAILED`/absent).

### 4.5 WireGuard (`checks/wireguard.rs`)
genl WireGuard dump → per configured peer, `now - last_handshake > max_age` →
violation; optional "tx/rx not advancing since last sweep" (needs prev-state
memory in the check).

### 4.6 Diagnostics (`checks/diagnostics.rs`)
`ctx.diagnostics.find_bottleneck()` → if `score() > max_bottleneck_score`, one
violation carrying `location`/`bottleneck_type`/`recommendation` as labels; and/or
iterate `scan().issues` above `min_issue_severity`.

### 4.7 Metric threshold (`checks/metric.rs`) — enables GUI rule-promotion (D7)
A generic check over **any telemetry the sensor already produces**: `{ metric:
"<prefix>", op: ">", threshold: 90, source?: "<glob>" }`. Evaluated against the
sensor's own last-published values (the sensor keeps a small `HashMap<metric,
f64>` of recent points, or the check re-reads via nlink). This is the server-side
equivalent of the GUI's `AlertRule`, so a user can **promote** a GUI threshold to
a headless expectation ([Plan 08 §6](08-gui-command-channel.md)). Keep it simple:
exact-or-glob metric match + `ComparisonOp` reused from the frontend's enum
(move `ComparisonOp` into `zensight-common` so both sides share it).

## 5. Config schema (`config.rs`) — extends `NetlinkConfig.expectations`

```json5
expectations: {
  eval_interval_secs: 10,
  default_for_secs: 15,            // global debounce
  sockets: [
    { name: "sshd",    listen: 22, severity: "critical" },
    { name: "db-conn", established_to: "10.0.0.5:5432", min: 1,
      rtt_ms_max: 200, retransmits_max: 500, severity: "warning", for_secs: 30 },
    { name: "no-telnet", forbid_listen: 23, severity: "critical" },
  ],
  links:      [ { iface: "eth0", up: true, addr: "10.0.0.1/24", mtu: 9000, severity: "critical" } ],
  routes:     [ { default_via: "10.0.0.254", severity: "critical" },
                { to: "10.9.0.0/16", severity: "warning" } ],
  neighbors:  [ { ip: "10.0.0.254", reachable: true, severity: "warning" } ],
  wireguard:  [ { peer: "gw2", handshake_max_age_s: 180, severity: "warning" } ],
  diagnostics:{ max_bottleneck_score: 0.7, min_issue_severity: "warning" },
}
```

Each list entry deserializes into a concrete `Expectation` impl; `config.rs`
builds `Vec<Box<dyn Expectation>>`. Validation: parse `established_to` as
`SocketAddr`, `addr` as a CIDR, `to`/`default_via` as IP/prefix — fail config
load on bad input (consistent with existing sensor config validation).

**Hot-swap for GUI authoring (D8/Plan 08):** the `Evaluator` keeps its
`Vec<Box<dyn Expectation>>` behind an `RwLock` and exposes
`replace_expectations(ExpectationsConfig)`, `add(ExpectationSpec)`,
`remove(rule: &str)`, and a `status() -> ExpectationStatus` snapshot. These back
the `expectations` command topic so the GUI can edit the live set without
restarting the sensor. A pushed set is also persisted to a runtime overlay file
(precedence: runtime overlay > on-disk config). The same config types must
therefore be `Serialize` (not just `Deserialize`) so the GUI can round-trip them.

## 6. Embedding in the netlink sensor (Plan 03 main.rs)

```rust
if let Some(exp_cfg) = config.netlink.expectations.clone() {
    let reporter = AlertReporter::new(hostname.clone(), Protocol::Netlink,
        runner.publisher(), Format::Json).with_debounce(exp_cfg.default_for());
    let evaluator = Evaluator::build(exp_cfg, reporter, EvalCtx::new(&hostname)?).await?;
    let handle = evaluator.handle();          // for on_event
    runner.spawn(evaluator.run());
    // forward netlink events to the evaluator for instant re-eval
    runner.spawn(forward_events_to(handle));
}
```

## 7. Tests
- Each check: feed synthetic observed-state (mock `SocketInfo`/`Link`/route
  vectors) into the check's pure inner function and assert `Vec<Violation>`.
  Extract the comparison logic from the I/O so it's testable without a kernel:
  `fn check_listening(listening: &HashSet<u16>, want: u16) -> Option<Violation>`.
- Evaluator lifecycle: a fake `Expectation` that flips satisfied↔violated across
  sweeps; assert `AlertReporter` sees firing then resolved (capture publishes).
- Error path: evaluate() returns `Err` → assert no resolve happens.

## 8. Acceptance criteria
- With `sockets: [{name:"sshd", listen:22}]` and sshd stopped, a critical
  `Expectation` alert appears at `zensight/netlink/<host>/@/alerts/<key>` and in
  the frontend Alerts view + a toast; starting sshd resolves it (row clears,
  recovery toast).
- `established_to` with the DB down fires; with N connections up, resolves.
- `default_via` mismatch fires on a route change **within ~1s** (event-driven),
  not only on the next interval.
- Debounce: a sub-`for:` blip does not fire.
- All check unit tests green; example config + README section added.
