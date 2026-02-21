# ZenSight Deep Analysis Report

**Date:** 2026-02-21
**Version analyzed:** 0.4.0 (commit `7c55d1b`)
**Scope:** Full workspace (frontend, common, bridge framework, 6 bridges, 2 exporters)

---

## Table of Contents

1. [Critical Bugs](#1-critical-bugs)
2. [High-Severity Issues](#2-high-severity-issues)
3. [Medium-Severity Issues](#3-medium-severity-issues)
4. [Performance Issues](#4-performance-issues)
5. [UI/UX Issues](#5-uiux-issues)
6. [Improvement Suggestions](#6-improvement-suggestions)
7. [Feature Ideas](#7-feature-ideas)
8. [Summary](#8-summary)

---

## 1. Critical Bugs

### 1.1 Missing `Sysinfo` Protocol in Key Expression Parsing

**File:** `zensight-common/src/keyexpr.rs:192-199`

The `parse_key_expr()` function is missing the `Sysinfo` variant in its protocol match:

```rust
let protocol = match parts[1] {
    "snmp" => Protocol::Snmp,
    "syslog" => Protocol::Syslog,
    "gnmi" => Protocol::Gnmi,
    "netflow" => Protocol::Netflow,
    "opcua" => Protocol::Opcua,
    "modbus" => Protocol::Modbus,
    _ => return None,  // <-- "sysinfo" falls through here!
};
```

**Impact:** Any Zenoh key expression for sysinfo (e.g., `zensight/sysinfo/server01/cpu/usage`) will fail to parse, returning `None`. This breaks the sysinfo pipeline in any component that relies on `parse_key_expr()`.

**Fix:** Add `"sysinfo" => Protocol::Sysinfo,` to the match arm.

---

### 1.2 Unsafe `transmute` in AdvancedPublisherRegistry

**File:** `zensight-bridge-framework/src/advanced_publisher.rs:183`

```rust
// Safety: We're using 'static lifetime because the publisher is stored
// in the registry and the session is kept alive by Arc
let publisher: AdvancedPublisher<'static> = unsafe { std::mem::transmute(publisher) };
```

**Impact:** This transmutes a borrowing lifetime to `'static`, violating Rust's memory safety guarantees. If the `Arc<Session>` is ever dropped before the publisher (e.g., during error recovery or shutdown), this creates a use-after-free. The safety comment's reasoning is insufficient -- `Arc` keeps the session alive, but the publisher may hold references to session internals whose lifetimes aren't governed by the `Arc`.

**Fix:** Either use a proper lifetime parameter on the registry struct, or restructure to use owned types.

---

### 1.3 `TelemetryValue` Untagged Enum Deserialization Ambiguity

**File:** `zensight-common/src/telemetry.rs:60-77`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TelemetryValue {
    Counter(u64),
    Gauge(f64),
    Text(String),
    Boolean(bool),
    Binary(Vec<u8>),
}
```

**Impact:** With `#[serde(untagged)]`, serde tries variants in order. A JSON integer like `42` will always deserialize as `Counter(42)`, never `Gauge(42.0)`. A JSON float `42.0` becomes `Gauge(42.0)`, but `42` (no decimal) becomes `Counter(42)` -- the caller has no control over which type they get. This leads to silent type confusion between Counters and Gauges depending on serialization format. Additionally, `true`/`false` as JSON booleans will match `Boolean`, but a JSON array of bytes `[1,2,3]` could ambiguously match either `Binary` or fail.

**Fix:** Use externally tagged serialization (`#[serde(tag = "type", content = "value")]`) or an adjacently tagged format to preserve type intent.

---

## 2. High-Severity Issues

### 2.1 `i64` to `f64` Precision Loss in Conversion

**File:** `zensight-common/src/telemetry.rs:85-88`

```rust
impl From<i64> for TelemetryValue {
    fn from(v: i64) -> Self {
        TelemetryValue::Gauge(v as f64)  // loses precision for |v| > 2^53
    }
}
```

**Impact:** Values larger than 2^53 (9,007,199,254,740,992) lose precision. While timestamps in milliseconds (current ~1.7 * 10^12) are safe, SNMP Counter64 values or large byte counts from NetFlow can exceed this threshold.

---

### 2.2 Race Condition in Publisher Cache (TOCTOU)

**File:** `zensight-bridge-framework/src/advanced_publisher.rs:156-191`

The double-checked locking pattern releases the read lock before acquiring the write lock. Between these two operations, another task could create the same publisher, resulting in duplicate publishers where one overwrites the other (potentially while the overwritten one has operations in flight).

---

### 2.3 `errors_last_hour` Counter Never Resets

**File:** `zensight-bridge-framework/src/health.rs:38, 283`

Despite the name `errors_last_hour`, the counter only increments and never resets. It actually represents `total_errors_since_startup`, not a rolling window. The frontend displays this as "Errors/hr" which is misleading.

---

### 2.4 Silent Metric Rendering Failures (Prometheus Exporter)

**File:** `zensight-exporter-prometheus/src/collector.rs:386-415`

All `writeln!(output, ...)` calls use `.ok()`, silently discarding write errors. If buffer allocation fails or a write error occurs, metrics are silently dropped from the `/metrics` output with no logging or alerting.

---

### 2.5 Silent Export Failures (OTEL Exporter)

**File:** `zensight-exporter-otel/src/exporter.rs:250-296, 333`

Metric recording and log emission errors are completely ignored. The exporter reports success in its stats even when the OTLP backend rejects data.

---

### 2.6 Unbounded Gauge HashMap (OTEL Exporter)

**File:** `zensight-exporter-otel/src/exporter.rs:101, 145`

The gauge storage `HashMap` grows without bound. No staleness check, no size limit, no cleanup. For long-running exporters with high-cardinality metrics, this is a memory leak.

---

### 2.7 gNMI Reconnection Loop Without Backoff

**File:** `zenoh-bridge-gnmi/src/subscriber.rs:43-56`

The gNMI subscriber reconnects every 5 seconds forever on failure, with no exponential backoff, attempt counter, or circuit breaker. Under sustained failure, this generates aggressive reconnection attempts and fills logs.

---

## 3. Medium-Severity Issues

### 3.1 Key Expression Validation Missing

**Files:** `zensight-bridge-framework/src/publisher.rs:50-56`, `zensight-common/src/keyexpr.rs:44-51`

Neither `Publisher::build_key()` nor `KeyExprBuilder::build()` validate inputs. Empty strings, special characters, or double slashes create invalid Zenoh key expressions silently.

### 3.2 `parse_key_expr()` Returns `Option` Instead of `Result`

**File:** `zensight-common/src/keyexpr.rs:185`

Returns `None` with no context on why parsing failed. Debugging key expression issues requires manual inspection. Should return `Result<ParsedKeyExpr, ParseError>`.

### 3.3 String-Typed Status Fields

**File:** `zensight-common/src/health.rs:40-41, 142`

`HealthSnapshot.status` and `BridgeInfo.status` are `String` instead of an enum. No compile-time validation, allowing typos like `"healhty"` or casing inconsistencies.

### 3.4 Lock Poisoning Not Handled

**File:** `zensight-bridge-framework/src/correlation.rs:143-155`

Multiple `.unwrap()` on `RwLock` acquisitions. If a thread panics while holding the lock, all subsequent lock acquisitions will panic too, crashing the bridge.

### 3.5 SNMP Lock Held Across Await

**File:** `zenoh-bridge-snmp/src/poller.rs:187-190, 224-229`

Mutex lock is held across `timeout()` and network I/O operations. This can cause lock contention and potential deadlocks under load.

### 3.6 Modbus Address Overflow

**File:** `zenoh-bridge-modbus/src/poller.rs:122`

`register.address + addr_offset as u16` could overflow with no bounds checking.

### 3.7 No Graceful Shutdown in Bridge Framework

**File:** `zensight-bridge-framework/src/runner.rs:213-238`

Worker tasks are aborted (`task.abort()`) without cancellation tokens. Open connections, in-flight messages, and pending I/O are not flushed.

### 3.8 Syslog `glob_to_regex()` Incomplete Escaping

**File:** `zenoh-bridge-syslog/src/filter.rs:123-141`

The glob-to-regex conversion doesn't escape all regex metacharacters, potentially causing incorrect pattern matching.

### 3.9 gNMI Nanosecond Conversion Overflow

**File:** `zenoh-bridge-gnmi/src/subscriber.rs:176`

`sub.sample_interval_ms * 1_000_000` can overflow for intervals > ~106 days. Same issue at line 178.

---

## 4. Performance Issues

### 4.1 `Vec::remove(0)` for History Trimming (O(n))

**File:** `zensight/src/view/device.rs:103, 121`

```rust
while history.len() > max_history {
    history.remove(0);  // O(n) shift of all elements
}
```

Called on every telemetry update in the hot path. With `max_history` of 1000 and many metrics per device, this causes unnecessary memory copies.

**Fix:** Use `VecDeque` for O(1) `pop_front()`.

### 4.2 Excessive Cloning in Telemetry Hot Path

**File:** `zensight/src/app.rs` (multiple locations)

`DeviceId`, `TelemetryPoint`, `ZenohConfig`, and `HashMap` keys are cloned on every metric update. For high-throughput scenarios (thousands of metrics/second), this creates significant allocation pressure.

### 4.3 Dashboard Filtering Re-runs on Every Tick

**File:** `zensight/src/view/dashboard.rs:219-228`

`filtered_devices()` is called repeatedly without caching filtered results. With 1000+ devices, this is wasteful.

### 4.4 String Allocations in Subscription Parsing

**File:** `zensight/src/subscription.rs:166-184`

`parse_bridge_liveliness()` allocates strings from key expressions on every sample. In the high-frequency path, these could be parsed with borrowed references instead.

### 4.5 NetFlow Mutex Serializes Packet Processing

**File:** `zenoh-bridge-netflow/src/receiver.rs:103`

`parser.lock().await` serializes all packet processing per exporter through a single mutex, creating a bottleneck for high-throughput NetFlow streams.

### 4.6 OTEL Meter/Logger Created on Every Metric

**File:** `zensight-exporter-otel/src/exporter.rs:247, 308`

`meter_provider.meter("zensight")` and `logger_provider.logger("zensight.syslog")` are called on every metric/log instead of being cached.

---

## 5. UI/UX Issues

### 5.1 No Loading Indicator During Zenoh Connection

The frontend shows no visual feedback during the 5-second connection timeout. Users may think the app is frozen.

### 5.2 View Transition Animations Not Implemented

**File:** `zensight/src/app.rs:936`

A transition key exists but is unused. View changes are instant with no visual continuity.

### 5.3 No Visual Indicator for Stale Metrics

Charts display old and fresh data with the same visual treatment. Users cannot tell which metrics have stopped updating.

### 5.4 Export Errors Not Surfaced

**File:** `zensight/src/app.rs:1100-1105`

CSV/JSON export failures are logged but not shown to the user.

### 5.5 Syslog Filters Only Applied Locally

**File:** `zensight/src/app.rs:752`

`// TODO: Send filter command to bridge via Zenoh` -- filters configured in the frontend are not propagated to the bridge. Users may not realize filtering happens only on the display side.

### 5.6 No Error/Toast Notification System

Settings save errors, connection failures, and export errors are only logged to the terminal. The app needs an in-app notification system.

---

## 6. Improvement Suggestions

### 6.1 Architecture

- **Add config hot-reload**: Bridges and exporters load config once at startup. Add `notify`-based file watching for runtime reconfiguration without restarts.
- **Implement backpressure**: `Publisher::publish_batch()` has no flow control. Add optional rate limiting and circuit breaker patterns.
- **Standardize error handling**: Mix of `anyhow`, `thiserror`, and ad-hoc error types. Consolidate on a consistent strategy per crate boundary.
- **Replace unsafe transmute**: The `AdvancedPublisherRegistry` should use safe lifetime management.

### 6.2 Code Quality

- **Add `clippy::pedantic`**: The workspace uses standard clippy. Enabling pedantic would catch many of the identified issues.
- **Audit `unwrap()` calls**: Production code (outside tests) should use `?` or `.expect("reason")` with meaningful messages.
- **Unify serialization**: Use typed `Format` enum consistently instead of string-based format selection in configs.
- **Add integration tests for bridges**: Currently only SNMP has integration tests. Syslog, Modbus, Netflow, Sysinfo, and gNMI bridges lack them.

### 6.3 Observability

- **Add internal metrics**: Bridges and exporters should expose their own health metrics (messages processed, errors, latency) through Zenoh.
- **Structured logging**: Some bridges mix `tracing::warn!` and `tracing::error!` inconsistently. Add a logging severity guide.
- **Implement proper rolling window for error counts**: Replace the unbounded `errors_last_hour` counter with time-bucketed tracking.

### 6.4 Security

- **Add authentication to exporter endpoints**: The Prometheus `/metrics` and OTEL endpoints are unauthenticated.
- **Validate SNMP community strings**: Currently stored as plain text in config. Consider supporting secrets from environment variables or vault.
- **TLS for gNMI**: The gNMI bridge should validate TLS certificates properly and support mTLS.

---

## 7. Feature Ideas

### 7.1 High Value / Medium Effort

| Feature | Description |
|---------|-------------|
| **Alert Forwarding** | Forward triggered alerts to external systems (Slack, PagerDuty, email) via webhooks |
| **Metric Persistence** | Store time-series data locally (SQLite/RocksDB) for historical queries beyond the in-memory window |
| **Dashboard Layouts** | Let users save/load custom dashboard layouts with pinned devices and preferred views |
| **Multi-Instance Correlation** | Cross-reference the same physical device seen by different bridges (SNMP + Syslog + Sysinfo) using IP/hostname correlation from the framework |

### 7.2 High Value / Higher Effort

| Feature | Description |
|---------|-------------|
| **Anomaly Detection** | Statistical anomaly detection (z-score, moving average deviation) on metric streams with auto-generated alerts |
| **Playback Mode** | Record telemetry sessions and replay them for post-mortem analysis |
| **Remote Dashboard** | Web-based dashboard (via zenoh-plugin-rest or WebSocket) for remote monitoring without the desktop app |
| **Plugin System** | Allow third-party bridge plugins to be loaded dynamically without recompiling |

### 7.3 Nice to Have

| Feature | Description |
|---------|-------------|
| **Dark/Light Theme Scheduling** | Auto-switch theme based on time of day |
| **Keyboard Shortcuts** | Vim-style navigation (`j`/`k` for device list, `/` for search, `Esc` to go back) |
| **Metric Annotations** | Let users annotate specific time points on charts (e.g., "deployed v2.3") |
| **CSV/JSON Import** | Import historical data from files for offline analysis |
| **Bulk Device Actions** | Select multiple devices for comparison, export, or group assignment |
| **Threshold Templates** | Predefined alert threshold templates per protocol (e.g., "CPU > 90% for 5 min") |
| **Bridge Auto-Discovery UI** | Show connected bridges with their versions, configs, and allow restart/reconfigure from the frontend |
| **Syslog Live Tail** | Real-time syslog stream view with grep-like filtering and highlighting |
| **NetFlow Geolocation** | Map IP addresses to geographic locations for flow visualization on a world map |
| **SNMP MIB Browser** | Built-in MIB tree browser with OID lookup and description display |

---

## 8. Summary

### Bug Count by Severity

| Severity | Count | Key Examples |
|----------|-------|--------------|
| **Critical** | 3 | Missing sysinfo parsing, unsafe transmute, untagged enum ambiguity |
| **High** | 7 | i64 precision loss, TOCTOU race, silent failures, memory leak |
| **Medium** | 9 | Missing validation, lock poisoning, overflow risks |
| **Performance** | 6 | O(n) history trimming, excessive cloning, mutex bottlenecks |
| **UI/UX** | 6 | Missing loading states, no error notifications |

### Top 5 Priority Fixes

1. **Add `"sysinfo"` to `parse_key_expr()`** -- One-line fix for a data pipeline bug
2. **Remove or replace `unsafe transmute`** -- Memory safety violation
3. **Tag `TelemetryValue` enum for serialization** -- Data integrity issue (breaking change, needs migration)
4. **Use `VecDeque` for metric history** -- Simple swap for measurable performance gain
5. **Add error logging to exporter write paths** -- Silent data loss in production

### Overall Assessment

ZenSight is a well-architected project with clean separation between bridges, common types, and the frontend. The codebase is organized logically, tests cover most UI flows, and the bridge framework provides good abstractions. The most impactful issues are:

- A few data integrity bugs in the common serialization layer that affect all components
- Silent error handling patterns in exporters that hide production issues
- One unsafe block that should be eliminated
- Performance patterns (Vec instead of VecDeque, excessive cloning) that will matter at scale

None of the issues are show-stoppers for current usage, but the critical bugs (especially 1.1 and 1.3) should be addressed before any production deployment.
