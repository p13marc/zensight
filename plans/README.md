# ZenSight Improvement Plans

Progress tracker for issues identified in [`ANALYSIS.md`](../ANALYSIS.md).

---

## Plan 01: Critical Bug Fixes [Immediate]

> [Detailed plan](./01-critical-bugfixes.md)

- [x] **1.1** Add missing `Sysinfo` protocol to `parse_key_expr()` (keyexpr.rs)
- [x] **1.2** Remove unsafe `transmute` in AdvancedPublisherRegistry (advanced_publisher.rs)
- [x] **1.2b** Fix TOCTOU race condition in publisher cache (advanced_publisher.rs)
- [x] **1.3** Tag `TelemetryValue` enum for unambiguous serialization (telemetry.rs)
- [x] Verify: all workspace tests pass
- [x] Verify: no clippy warnings

---

## Plan 02: Data Integrity & Serialization [High Priority]

> [Detailed plan](./02-data-integrity.md)

- [x] **2.1** Fix `i64` to `f64` precision loss in `From<i64>` conversion (telemetry.rs)
- [x] **2.2** Change `parse_key_expr()` to return `Result` with descriptive errors (keyexpr.rs)
- [x] **2.3** Add input validation to `KeyExprBuilder::build()` and `Publisher::build_key()` (keyexpr.rs, publisher.rs)
- [x] **2.4** Replace string-typed status fields with `HealthStatus` enum (health.rs)
- [x] Verify: all workspace tests pass

---

## Plan 03: Bridge Framework Hardening [High Priority]

> [Detailed plan](./03-bridge-framework-hardening.md)

- [x] **3.1** Implement rolling window for `errors_last_hour` counter (health.rs)
- [x] **3.2** Handle lock poisoning gracefully in CorrelationRegistry (correlation.rs)
- [ ] ~~**3.3** Add graceful shutdown with cancellation tokens (runner.rs)~~ — skipped (too disruptive)
- [ ] ~~**3.4** Add backpressure support to Publisher (publisher.rs)~~ — skipped (too disruptive)
- [x] **3.5** Improve error categorization for Zenoh errors (error.rs)
- [x] Verify: all bridge tests pass

---

## Plan 04: Exporter Reliability [High Priority]

> [Detailed plan](./04-exporter-reliability.md)

- [x] **4.1** Fix silent metric rendering failures in Prometheus exporter (collector.rs)
- [x] **4.2** Fix silent export failures in OTEL exporter (exporter.rs)
- [x] **4.3** Add staleness-based cleanup for OTEL gauge HashMap (exporter.rs)
- [x] **4.4** Fix gauge key collision with sorted attributes (exporter.rs)
- [x] **4.5** Cache Meter/Logger instances in OTEL exporter (exporter.rs)
- [x] **4.6** Add decode failure metrics to both exporters (subscriber.rs)
- [x] Verify: all exporter tests pass

---

## Plan 05: Bridge Robustness [Medium Priority]

> [Detailed plan](./05-bridge-robustness.md)

- [x] **5.1** Add exponential backoff to gNMI reconnection loop (subscriber.rs)
- [x] **5.2** Fix gNMI nanosecond conversion overflow (subscriber.rs)
- [ ] ~~**5.3** Fix SNMP mutex held across await (poller.rs)~~ — skipped (future borrows session, requires deep restructure)
- [x] **5.4** Fix Modbus address overflow with checked arithmetic (poller.rs)
- [x] **5.5** Fix syslog `glob_to_regex()` incomplete escaping (filter.rs)
- [x] **5.6** Reduce NetFlow mutex contention (receiver.rs)
- [x] Verify: all bridge tests pass

---

## Plan 06: Frontend Performance & UX [Medium Priority]

> [Detailed plan](./06-frontend-performance-ux.md)

### Performance
- [x] **6.1** Replace `Vec` with `VecDeque` for metric history (device.rs)
- [x] **6.2** ~~Reduce cloning in telemetry hot path (app.rs)~~ — skipped (already minimal)
- [x] **6.3** Cache dashboard filtered results (dashboard.rs)
- [x] **6.4** Reduce string allocations in subscription parsing (subscription.rs)

### UX
- [x] **6.5** Add loading indicator during Zenoh connection (app.rs)
- [x] **6.6** Add toast notification system (new: toast.rs)
- [x] **6.7** Surface export errors to user via toast (app.rs)
- [x] **6.8** Add stale metric visual indicator (device.rs)
- [x] Verify: all frontend tests pass

---

## Plan 07: Feature Roadmap [Low Priority]

> [Detailed plan](./07-feature-roadmap.md)

### Phase 1: Foundation
- [ ] **7.1** Metric persistence with SQLite
- [ ] **7.2** Dashboard layouts (save/load)
- [ ] **7.3** Alert forwarding via webhooks

### Phase 2: Advanced
- [ ] **7.4** Multi-instance device correlation in frontend
- [ ] **7.5** Anomaly detection (z-score, moving average)
- [ ] **7.6** Playback mode (requires 7.1)

### Phase 3: Nice to Have
- [ ] **7.7** Keyboard shortcuts (vim-style navigation)
- [ ] **7.8** Metric annotations on charts
- [ ] **7.9** Threshold templates per protocol
- [ ] **7.10** Bulk device actions (multi-select)
- [ ] **7.11** Syslog live tail view
- [ ] **7.12** SNMP MIB browser
- [ ] **7.13** NetFlow geolocation map
- [ ] **7.14** Dark/light theme auto-scheduling

---

## Execution Order

```
Week 1:  Plan 01 (Critical Bugs)
         Plan 02 (Data Integrity)

Week 2:  Plan 03 (Framework Hardening)
         Plan 04 (Exporter Reliability)

Week 3:  Plan 05 (Bridge Robustness)
         Plan 06 (Frontend Perf & UX)

Week 4+: Plan 07 (Features - ongoing)
```

## How to Use

1. Work through plans in order (01 -> 07)
2. Check off items as they are completed
3. Run `cargo test --workspace` after each plan
4. Each plan file contains detailed implementation guidance
