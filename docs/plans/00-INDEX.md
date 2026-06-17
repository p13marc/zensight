# ZenSight Redesign: Sensors + Alerting — Master Plan

**Status:** ✅ **IMPLEMENTED** on branch `redesign/sensors-alerting` (Plans 01–11).
Both sensors verified live/pcap; sentinel + GUI authoring verified live; full
workspace green (excl. snmp/gnmi which need openssl/protoc). · revised after a
critical review pass (2026-06-15)
**Source proposal:** [`docs/SENSORS_NLINK_NETRING.md`](../SENSORS_NLINK_NETRING.md)
**Scope:** rename the crate family, add an alert model, add two new sensors
(`nlink`, `netring`), add an expectation/anomaly alert engine, **and fully wire
the GUI** (alerts, expectation authoring, specialized views, topology).

> Backward compatibility is **not** preserved. The project is a monorepo with no
> third-party wire consumers, so we take the clean cut: rename the wire channels
> too (see D9). Crate names, type names, the `Bridge*` vocabulary, and the
> `_meta/bridges` keys all change.

---

## 0. Review changelog (what the review pass changed)

The first draft (plans 00–07) was reviewed against the **actual** source. Four
material gaps surfaced; they drive the new plans and revised decisions:

1. **The GUI cannot send anything to sensors.** The Zenoh `Session` lives inside
   the subscription async-stream; the sync `app.update()` has no handle to it.
   The existing syslog "Apply to Bridge" button is a dead `// TODO` (app.rs:766).
   → **New decision D8** + **Plan 08** make GUI→sensor commands work at all.
2. **Expectations were file-only.** The user wants them in the GUI. The syslog
   `@/commands/*` + `@/status/*` queryable pattern already exists and generalizes
   cleanly. → **Plan 08** adds GUI authoring/push of expectations + detector
   tuning on top of D8.
3. **No specialized views; topology is hardcoded to `Sysinfo`.** netlink/netring
   carry rich per-device data (interfaces, sockets, flows, L7, anomalies) and
   real adjacency. → **Plan 09** (specialized views) and **Plan 10** (topology
   enrichment), and the GUI acceptance bar in §3 is raised.
4. **Two netring API claims were wrong.** netring exposes `ChannelSink` (mpsc of
   `OwnedAnomaly`) and typed `.subscribe(flow/session/packet).to(..)` handlers —
   no hand-rolled sink trait needed. → **Decision D10** + corrections in Plans
   05/06.

Also: **Plan 01 flips to a full wire rename** (D9), and **the alert model is
reframed** so server-side expectations are the durable source of truth and GUI
threshold-rules are demoted to an ephemeral convenience (D7).

---

## 1. The plans

| # | Plan | Crates touched | Depends on | Effort |
|---|------|----------------|-----------|--------|
| 01 | [Rename `bridge` → `sensor` (full wire cut)](01-rename-bridges-to-sensors.md) | all | — | M |
| 02 | [Alert model + command primitives](02-alert-model.md) | common, sensor-core, frontend | 01 | M |
| 03 | [`zensight-sensor-netlink`](03-sensor-netlink.md) | new + common | 01, 02 | M–L |
| 04 | [`zensight-sentinel` (expectations / Pillar B)](04-sentinel-expectations.md) | new + sensor-netlink | 02, 03 | M–L |
| 05 | [`zensight-sensor-netring`](05-sensor-netring.md) | new + common | 01, 02 | M–L |
| 06 | [Anomaly alerts (Pillar A)](06-anomaly-alerts.md) | sensor-netring + sentinel | 02, 05 | S–M |
| 07 | [Frontend: alert ingest + lifecycle](07-frontend.md) | zensight | 02 | M |
| **08** | [**GUI command channel + expectations authoring**](08-gui-command-channel.md) | common, sensor-core, sentinel, frontend | 02, 04, 07 | M |
| **09** | [**Specialized GUI views (netlink / netring / security)**](09-specialized-views.md) | zensight | 03, 05, 07 | M |
| **10** | [**Topology enrichment (measured edges + alert overlay)**](10-topology-enrichment.md) | zensight + sensor-netlink | 03, 05, 07 | M |
| **11** | [**Exporters surface alerts**](11-exporters-alerts.md) | exporters | 02 | S |

**Dependency graph:**

```
01 rename ─► 02 alert+cmd ─┬─► 07 frontend ─┬─► 08 gui-commands ─► (authoring)
                           │                 ├─► 09 specialized views
            ┌──────────────┤                 └─► 10 topology
            │              │
            ├─► 03 netlink ─► 04 sentinel (Pillar B) ─► (feeds 08, 10)
            ├─► 05 netring ─► 06 anomaly (Pillar A) ─► (feeds 09)
            └─► 11 exporters
```

**Suggested merge order:** 01 → 02 → 07 → 08 (GUI can now author + show alerts) →
03 → 04 → 05 → 06 → 09 → 10 → 11. Landing 07+08 early makes every later sensor
demoable *and controllable* from the GUI as soon as it ships.

---

## 2. Key design decisions (read before any plan)

### D1 — Naming
`bridge` → `sensor` (a process that observes and publishes); `sentinel` = the
expectation evaluator library. Full mapping in Plan 01.

### D2 — Two `Alert` representations, one display
- **`zensight_common::Alert`** — the wire type sensors publish (serde, no `iced`).
- **`zensight::view::alerts::Alert`** — the existing local threshold-rule alert.
Both render in one Alerts view; `common::AlertSeverity` maps 1:1 onto the
frontend `Severity`. See Plan 02/07.

### D3 — Alert lifecycle (firing → resolved), keyed
Per-alert key `zensight/<protocol>/@/alerts/<alert_key>`; `Put(Firing)` raises/
updates, `Put(Resolved)`+`Delete` clears. Frontend auto-closes the row and toasts
recovery. Mirrors the liveliness Put/Delete it already handles.

### D4 — Evaluation runs embedded in sensors
Expectation/anomaly logic runs in-process (needs raw `nlink`/`netring` state);
the *types* and *config schema* are shared. `AlertReporter` (sensor-core) owns
publishing + debounce; `zensight-sentinel` owns expectation evaluation.

### D5 — New `Protocol` variants
Add `Protocol::Netlink`, `Protocol::Netring`. Expectation alerts publish under
`netlink`; anomaly alerts under `netring`.

### D6 — Toolchain / platform
Rust 1.95+, edition 2024, **Linux-only** sensors (cfg-gated members).
`nlink` reads are unprivileged; `netring` capture needs `CAP_NET_RAW`
(+`CAP_IPC_LOCK` for AF_XDP). The sensors never write to the kernel.

### D7 — Expectations are the durable truth; GUI rules are ephemeral *(new)*
A GUI `AlertRule` only evaluates while the app is open and only on telemetry the
app happened to receive — fine for ad-hoc exploration, useless as real
monitoring. **Server-side expectations (Plan 04) run headless on each host and
fire regardless of any GUI.** So the GUI's primary alerting role becomes:
**author/manage server-side expectations (Plan 08)** and **display alerts**; the
local threshold-rule engine is demoted to a "live highlight" convenience. A GUI
rule may optionally be **promoted** to a pushed expectation (Plan 08 §6).

### D8 — GUI→sensor command architecture *(new — unblocks all GUI control)*
The subscription stream must hand the session up to the app:
- On connect, yield `Message::Connected(Arc<zenoh::Session>)` (today it yields a
  unit `Connected`). The app stores `Option<Arc<Session>>`.
- `app.update()` issues commands via
  `iced::Task::future(async move { session.put(key, payload).await; ... })`,
  optionally awaiting a queryable reply for status.
- Generalize the syslog `@/commands/<topic>` + `@/status/<topic>` pattern into a
  shared `zensight-common::command` module (`Command<T>` envelope + `command_key`
  / `status_key` builders). This **fixes the existing syslog TODO** and powers
  expectation authoring + detector tuning.

### D9 — Wire rename goes all the way *(new — supersedes Plan 01 Option A)*
No third-party wire consumers ⇒ take the clean cut in one release:
`_meta/bridges/*` → `_meta/sensors/*`; the `HealthSnapshot.bridge` field →
`sensor`; any `bridge` string in payloads → `sensor`. The
`zensight/<protocol>/<source>/...` telemetry prefix is unchanged (it was never
`bridge`-named). Atomic cutover across all sensors + frontend in the rename
commit. (Option A — keep wire stable — remains documented as the low-risk
fallback if a staged rollout is ever needed.)

### D10 — netring integration via `ChannelSink` + typed subscriptions *(new)*
Do **not** hand-roll a sink trait. Use netring's real API:
- **Anomalies →** `.sink(ChannelSink)` → mpsc of `OwnedAnomaly` → drained by a
  task that maps to `common::Alert` and calls `AlertReporter`.
- **Telemetry →** `.subscribe(flow::<Tcp>()…/session::<Tls>()…/packet()…).to(closure)`,
  `.export_flows(exporter)`, `.export_active_timeout(..)`, and `.tick(period, ..)`
  for periodic aggregates; closures push into an mpsc drained by a publisher task
  (keeps netring's zero-alloc hot path non-blocking).
- **Detectors →** `.detect(pattern_detector!(PortScanDetector|BeaconDetector|DgaScorer))`.
- **Self-health →** `capture_metrics` / `MonitorHealth`.
Corrects Plans 05/06.

---

## 3. Cross-cutting acceptance criteria

A plan is "done" when, in addition to its own criteria:

1. `cargo build --workspace` + `cargo clippy --workspace` clean on Linux;
   `cargo fmt --all --check` passes.
2. New code has unit tests; the workspace test count in `CLAUDE.md`/`MEMORY.md`
   is updated.
3. New wire types round-trip through `encode`/`decode_auto` in a test.
4. New config has an example in `configs/` and is documented in the crate README.
5. `CLAUDE.md` (root + crate), `ARCHITECTURE.md`, `README.md` updated.
6. **GUI completeness (raised bar):** any new data the platform produces is
   reachable in the desktop app — as a dashboard device, a specialized view
   (Plan 09), an alert (Plan 07), a topology node/edge (Plan 10), and — where it
   is user-controllable — an authoring surface (Plan 08). "Headless-only" is not
   done. New protocols must have icons and appear in every exhaustive `match
   Protocol`.

---

## 4. Risk register

| Risk | Mitigation | Plan |
|---|---|---|
| Rename churns every crate + the wire | One commit, mechanical; atomic cutover; CI gate | 01 |
| GUI→sensor session-send is a latent architectural gap | Fix it first (D8) before any GUI control feature | 08 |
| Two `Alert` types confuse contributors | Name + document; one adapter; D7 clarifies roles | 02, 07 |
| Alert flapping spams toasts | `for:` debounce + dedup in `AlertReporter`; resolved auto-clear | 02, 04 |
| High-cardinality alert keys / flow keys | ids in labels not series; bucket alert_key by `(rule, src)`; flow subtree sampled/expired | 02, 05, 06, 11 |
| Linux-only sensors break macOS/Windows CI | `cfg(target_os="linux")` members + CI matrix | 03, 05 |
| `nlink`/`netring` pre-1.0 API churn | Pin exact versions; wrap their types behind thin sensor adapters; method names in plans flagged "verify vs pinned version" | 03, 05 |
| netring needs `CAP_NET_RAW`/root in CI | Unit-test mapping on synthetic data; gate live tests behind `live`/`caps` feature | 05, 06 |
| Authoring expectations from GUI without auth = remote control of hosts | Command channel is a security surface: document Zenoh access-control scoping; expectations are advisory (read-only checks), but the channel must be access-controlled | 08 |

---

## 5. Out of scope (explicitly deferred)

- **Cross-host / fleet sentinel** (a daemon subscribing to telemetry to assert
  fleet-wide expectations). The embedded design (D4) is forward-compatible.
- **Active/synthetic probing.** All Pillar-B checks are passive reads.
- **Writing/remediation** (nlink `apply`, shaping). Sensors are read-only.
- **Shared/persisted alert acknowledgement across GUIs** (ack is local today;
  Plan 07 §notes a future publish-ack channel).
- **Authn/authz on the command channel** beyond documenting Zenoh ACL scoping
  (Plan 08 raises it; a full security model is its own effort).
