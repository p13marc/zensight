# Plan 04: Exporter Reliability

**Priority:** High
**Estimated effort:** 2-3 days
**Risk:** Low-Medium (isolated to exporter crates)
**Crates affected:** `zensight-exporter-prometheus`, `zensight-exporter-otel`

---

## Objective

Eliminate silent data loss in both exporters, fix memory leaks, and add proper error observability.

---

## Task 1: Fix Silent Metric Rendering Failures (Prometheus)

**Ref:** Analysis 2.4
**File:** `zensight-exporter-prometheus/src/collector.rs:386-415`

### Problem

All `writeln!(output, ...)` calls use `.ok()`, silently discarding write errors.

### Implementation

1. Replace `.ok()` with proper error handling:

```rust
// BEFORE:
writeln!(output, "# TYPE {} {}", metric_name, type_str).ok();

// AFTER:
if let Err(e) = writeln!(output, "# TYPE {} {}", metric_name, type_str) {
    tracing::error!(metric = %metric_name, error = %e, "Failed to write metric");
    self.render_errors.fetch_add(1, Ordering::Relaxed);
}
```

2. Add a `render_errors` counter to the collector for observability.
3. Expose render error count on the `/health` endpoint.

---

## Task 2: Fix Silent Export Failures (OTEL)

**Ref:** Analysis 2.5
**File:** `zensight-exporter-otel/src/exporter.rs:250-296, 333`

### Problem

Metric recording and log emission errors are completely ignored.

### Implementation

1. Add error logging to `record_metric()`:

```rust
pub fn record_metric(&self, point: &TelemetryPoint) {
    match self.try_record_metric(point) {
        Ok(()) => self.stats.metrics_exported.fetch_add(1, Ordering::Relaxed),
        Err(e) => {
            tracing::warn!(
                metric = %point.metric,
                source = %point.source,
                error = %e,
                "Failed to record OTEL metric"
            );
            self.stats.metrics_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

2. Add a `metrics_failed` counter to exporter stats.
3. Same treatment for `record_log()` / `logger.emit()`.

---

## Task 3: Fix Unbounded Gauge HashMap (OTEL)

**Ref:** Analysis 2.6
**File:** `zensight-exporter-otel/src/exporter.rs:101, 145`

### Problem

Gauge HashMap grows without bound -- memory leak for long-running exporters.

### Implementation

1. Add a `last_updated` timestamp to gauge entries:

```rust
struct GaugeEntry {
    value: f64,
    last_updated: Instant,
}
```

2. Add a periodic cleanup task (every 5 minutes):

```rust
async fn cleanup_stale_gauges(gauges: Arc<Mutex<HashMap<String, GaugeEntry>>>, max_age: Duration) {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    loop {
        interval.tick().await;
        let mut map = gauges.lock().await;
        let before = map.len();
        map.retain(|_, entry| entry.last_updated.elapsed() < max_age);
        let removed = before - map.len();
        if removed > 0 {
            tracing::info!(removed, remaining = map.len(), "Cleaned up stale gauges");
        }
    }
}
```

3. Add a configurable `staleness_timeout` (default: 15 minutes).
4. Add a `max_gauge_series` limit (default: 100,000).

---

## Task 4: Fix Gauge Key Collision (OTEL)

**Ref:** Analysis exporter issue (gauge key generation)
**File:** `zensight-exporter-otel/src/exporter.rs:269-277`

### Problem

Gauge key is fragile: `format!("{}:{}", metric_name, attrs.join(","))` -- attribute ordering is not sorted, and separators can collide with values.

### Implementation

1. Sort attributes before key generation:

```rust
fn build_gauge_key(metric_name: &str, attributes: &[KeyValue]) -> String {
    let mut sorted_attrs: Vec<_> = attributes
        .iter()
        .map(|kv| format!("{}={}", kv.key, kv.value))
        .collect();
    sorted_attrs.sort();
    format!("{}\x00{}", metric_name, sorted_attrs.join("\x00"))
    // Use null byte as separator (can't appear in metric names or values)
}
```

---

## Task 5: Cache Meter and Logger Instances (OTEL)

**Ref:** Analysis 4.6
**File:** `zensight-exporter-otel/src/exporter.rs:247, 308`

### Problem

`meter_provider.meter("zensight")` and `logger_provider.logger("zensight.syslog")` called on every metric/log.

### Implementation

Store meter and logger as fields:

```rust
pub struct OtelExporter {
    meter: Meter,
    logger: Logger,
    // ... existing fields ...
}

impl OtelExporter {
    pub fn new(/* ... */) -> Self {
        let meter = meter_provider.meter("zensight");
        let logger = logger_provider.logger("zensight.syslog");
        Self { meter, logger, /* ... */ }
    }
}
```

---

## Task 6: Add Decode Failure Metrics (Both Exporters)

**Ref:** Analysis shared issue (payload decoding)
**Files:** `subscriber.rs` in both exporters

### Problem

JSON/CBOR decode failures are logged at `debug!` with no metrics.

### Implementation

Add counters:

```rust
pub struct SubscriberStats {
    pub samples_received: AtomicU64,
    pub samples_decoded: AtomicU64,
    pub decode_failures: AtomicU64,  // <-- ADD
}
```

Log at `warn!` instead of `debug!`:

```rust
None => {
    tracing::warn!(key = %key_expr, "Failed to decode payload as JSON or CBOR");
    stats.decode_failures.fetch_add(1, Ordering::Relaxed);
}
```

---

## Validation

```bash
cargo test -p zensight-exporter-prometheus
cargo test -p zensight-exporter-otel
cargo clippy -p zensight-exporter-prometheus -p zensight-exporter-otel -- --deny warnings
```

## Success Criteria

- [ ] Prometheus render errors are logged and counted
- [ ] OTEL export failures are logged and counted
- [ ] OTEL gauge HashMap has staleness-based cleanup
- [ ] Gauge keys are collision-resistant (sorted, safe separators)
- [ ] Meter/Logger instances are cached, not recreated per metric
- [ ] Decode failures tracked in subscriber stats
- [ ] All exporter tests pass
