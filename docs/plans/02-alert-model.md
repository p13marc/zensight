# Plan 02 — Alert model (`zensight-common` + `sensor-core`)

**Goal:** introduce a **wire alert type** that sensors publish, an `AlertReporter`
that manages alert lifecycle (firing → resolved, dedup, debounce), and the
subscription plumbing so the frontend receives them. This is the shared substrate
for both Pillar A (anomalies, Plan 06) and Pillar B (expectations, Plan 04).

**Depends on:** 01. **Effort:** M.

See [00-INDEX D2/D3/D4/D7](00-INDEX.md#2-key-design-decisions-read-before-any-plan)
for why there are two `Alert` types and how lifecycle works.

> **Framing (D7):** the `common::Alert` introduced here is the **durable**,
> headless alert — produced by a sensor/sentinel and true whether or not any GUI
> is running. The frontend's existing threshold-`AlertRule` engine is the
> *ephemeral* convenience (only fires while the app is open). This plan builds the
> durable path; Plan 08 lets the GUI author what produces it.

> **Command primitives:** the `zensight-common::command` module (shared
> `command_key`/`status_key` builders + `Command<T>` envelope) is specified in
> [Plan 08 §2](08-gui-command-channel.md). It's listed under this plan in the
> INDEX because it's a `common`/`sensor-core` primitive, but its consumers are in
> Plan 08. Land the module here if convenient, or with Plan 08 — either works.

---

## 1. New module: `zensight-common/src/alert.rs`

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::Protocol;

/// Severity of a sensor-emitted alert. Plain (no `iced`); maps 1:1 onto the
/// frontend `view::alerts::Severity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default,
         Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    #[default]
    Warning,
    Critical,
}

/// What produced the alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// Pillar A — netring detector (port scan, beacon, DGA, …).
    Anomaly,
    /// Pillar B — an expectation about machine state was violated.
    Expectation,
    /// The sensor's own health (e.g. capture drop-rate) crossed a line.
    SensorHealth,
}

/// Firing vs resolved. Drives auto-clear in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    #[default]
    Firing,
    Resolved,
}

/// A fully-formed, sensor-decided alert. THE WIRE TYPE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Unix epoch millis of the latest state transition.
    pub timestamp: i64,
    /// Host / sensor identifier (the same value used as `source` in telemetry).
    pub source: String,
    /// Namespace the alert lives under (`netlink` for expectations, `netring`
    /// for anomalies). Also the `<protocol>` key segment.
    pub protocol: Protocol,
    pub kind: AlertKind,
    /// Stable rule identifier, e.g. "ssh-listening" or "PortScanDetector".
    pub rule: String,
    pub severity: AlertSeverity,
    pub state: AlertState,
    /// Human-readable one-liner for the alert row / toast.
    pub summary: String,
    /// Structured context (ip, port, peer, sni, expected, actual, …).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl Alert {
    pub fn new(source: impl Into<String>, protocol: Protocol, kind: AlertKind,
               rule: impl Into<String>, severity: AlertSeverity,
               summary: impl Into<String>) -> Self { /* ts = now, state = Firing */ }

    pub fn with_label(mut self, k: impl Into<String>, v: impl Into<String>) -> Self { /* … */ }
    pub fn resolved(mut self) -> Self { self.state = AlertState::Resolved; self }

    /// Stable key segment: hash of (rule + sorted labels). Used as the last
    /// key-expr segment so updates to the same logical alert replace in place.
    pub fn alert_key(&self) -> String { /* short hex of a stable hash */ }
}
```

Re-export from `lib.rs`:
`pub use alert::{Alert, AlertKind, AlertSeverity, AlertState};`

**Tests:** serde round-trip (JSON + CBOR via `encode`/`decode_auto`); `alert_key`
stability under label reordering; `resolved()` transition.

## 2. New key-expr helpers (`zensight-common/src/keyexpr.rs`)

```rust
impl KeyExprBuilder {
    /// `zensight/<protocol>/@/alerts/<alert_key>`
    pub fn alert_key_expr(&self, alert_key: &str) -> String { /* … */ }
}

/// `zensight/*/@/alerts/**` — frontend subscribes (covered by `zensight/**`
/// already, but add for explicit queries).
pub fn all_alerts_wildcard() -> String { format!("{KEY_PREFIX}/*/@/alerts/**") }
```

Extend `parse_key_expr` / document the new `@/alerts/<key>` channel.

## 3. New module: `zensight-sensor-core/src/alert.rs` — `AlertReporter`

Owns publishing + lifecycle. Mirrors how `BridgeHealth`/`LivelinessManager`
already wrap a `Publisher`.

```rust
pub struct AlertReporter {
    source: String,
    protocol: Protocol,
    publisher: Publisher,            // from sensor-core
    format: Format,
    // alert_key -> (current state, first_seen, last_published)
    active: RwLock<HashMap<String, ActiveAlert>>,
    debounce: Duration,              // global default `for:`
}

struct ActiveAlert { severity: AlertSeverity, fired_at: Instant, published: bool }

impl AlertReporter {
    pub fn new(source, protocol, publisher, format) -> Self;
    pub fn with_debounce(self, d: Duration) -> Self;

    /// Report the *current* set of violations for a logical rule each tick.
    /// The reporter diffs against `active` and emits Put(Firing)/Put(Resolved)+Delete
    /// as state changes. `for_duration` overrides the global debounce per call.
    pub async fn observe(&self, alert: Alert, for_duration: Option<Duration>) -> Result<()>;

    /// Mark every previously-firing alert for `rule` that is NOT in `still_firing`
    /// as resolved. Call after a full evaluation sweep so cleared expectations clear.
    pub async fn reconcile(&self, rule: &str, still_firing: &[String]) -> Result<()>;

    /// Resolve+tombstone all active alerts (graceful shutdown).
    pub async fn resolve_all(&self) -> Result<()>;
}
```

**Lifecycle rules:**
- First `observe` of a key starts a debounce timer; only after `for_duration`
  continuously-firing does it `Put(state=Firing)`.
- A key absent from a sweep (via `reconcile`) → `Put(state=Resolved)` then a
  Zenoh `Delete` on the key (tombstone) so late subscribers don't see stale.
- Re-firing within a short window updates timestamp/severity without re-toasting
  (frontend dedups on `alert_key`).

Publishes to `publisher.publish_to_key(&builder.alert_key_expr(key), payload)`.

**Tests:** debounce (no publish before `for:`), firing→resolved on reconcile,
severity escalation updates in place, `resolve_all` tombstones everything.
Use a mock/in-proc Zenoh session or assert against a captured publish log.

## 4. New `Protocol` variants — see Plan 03/05

`Alert.protocol` needs `Protocol::Netlink`/`Protocol::Netring`; add them in those
plans (here we just reference them). Until then, tests use an existing variant.

## 5. Frontend subscription wiring (`zensight/src/subscription.rs`)

Extend `decode_sample` — in the `@` branch, add an `alerts` arm
(`segment3 == "alerts"`):

```rust
} else if segment3 == "alerts" {
    // zensight/<protocol>/@/alerts/<alert_key>
    return match decode_auto::<Alert>(payload) {
        Ok(alert) => Some(Message::AlertReceived(alert)),
        Err(e) => { tracing::warn!(error=%e, key=%key, "decode Alert"); None }
    };
}
```

Also handle the **Delete** tombstone: in the telemetry `select!` arm, check
`sample.kind() == SampleKind::Delete` for `@/alerts/` keys and yield
`Message::AlertCleared { alert_key }` (parse the key tail). (Today the telemetry
arm assumes Put; add a kind check for the alerts subtree.)

## 6. Frontend message + state (handed off to Plan 07)

This plan adds the **Message variants and decode**; Plan 07 does the view. Add:

```rust
// message.rs
Message::AlertReceived(zensight_common::Alert),
Message::AlertCleared { protocol: String, alert_key: String },
```

`app.rs::update` minimal handling (full UI in Plan 07): insert/remove into a new
`AlertsState.external: HashMap<String, common::Alert>` keyed by `alert_key`, and
toast on new Firing / on Resolved.

## 7. Acceptance criteria

- `common::Alert` round-trips JSON+CBOR; `alert_key` stable.
- `AlertReporter` unit tests pass (debounce, firing/resolved, reconcile, shutdown).
- Frontend decodes `@/alerts/<key>` Put → `AlertReceived`, Delete → `AlertCleared`.
- A throwaway integration test: publish an `Alert` via `AlertReporter` against a
  real in-process Zenoh peer, subscribe with the frontend decoder, assert the
  `Message` round-trips.
- Docs: new `@/alerts` channel added to `ARCHITECTURE.md` and root `CLAUDE.md`
  "Health & Liveness Data" section.

## 8. Notes for exporters

The Prometheus/OTEL exporters subscribe to `zensight/**` and currently expect
`TelemetryPoint`; they will now also see `@/alerts/*` payloads. Making them skip
(and optionally export) alerts is **[Plan 11](11-exporters-alerts.md)**. The
*skip-safety* part of Plan 11 (don't error on the new channel) is mandatory and
should land alongside this plan.
