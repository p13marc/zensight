# Plan 09 — Specialized GUI views (netlink / netring / security)

**Goal:** give the two new sensors first-class device views (like syslog/snmp/
netflow already have), plus a cross-cutting **Security** view for anomalies. Today
`specialized::specialized_view()` matches on `Protocol`; new protocols fall back
to the generic metric grid, which buries their structure.

**Depends on:** 03 (netlink data), 05 (netring data), 07 (alert ingest).
**Effort:** M. Pure frontend.

Pattern reference (verified): `zensight/src/view/specialized/mod.rs::specialized_view()`
matches `Protocol → *_device_view`; syslog is special-cased because it needs extra
state (`SyslogFilterState`). netlink and netring follow the same shape.

---

## 1. Register the new protocols

`view/specialized/mod.rs`:
```rust
match state.device_id.protocol {
    // ... existing ...
    Protocol::Netlink => None,   // needs ExpectationsState → handled like syslog
    Protocol::Netring => None,   // needs NetringViewState → handled like syslog
    Protocol::Opcua  => None,
}
```
Both need side state (expectation status / detector status), so route them through
a `device_view_with_*` dispatcher in `device.rs` exactly as syslog does, OR (better)
generalize: `device_view(state, &SpecializedStates)` where `SpecializedStates`
bundles `syslog_filter`, `expectations`, `netring` so the app passes one struct.

## 2. netlink device view (`view/specialized/netlink.rs`)

A host's kernel-network panel. Tabs/sections built from the telemetry the netlink
sensor publishes (Plan 03):

- **Interfaces** — table: name, oper/carrier, MTU, speed, rx/tx bytes & rates
  (derive rate from the existing history/`VecDeque`), errors/drops. Per-queue drop
  detail on expand. Sparkline per interface (reuse `view/chart.rs`).
- **Sockets** — the sockdiag aggregates: established/listen/time_wait counts;
  retransmit & max-RTT gauges.
- **Routes / neighbors** — recent route/neighbor *events* (from the event-stream
  telemetry) as a small log; gateway reachability badge.
- **Expectations strip** — inline summary of this host's expectations and their
  state (satisfied/firing), with a link to the full Expectations view (Plan 08).
- **WireGuard** (if present) — per-peer last-handshake age + rx/tx.

No new wire types — it's a richer rendering of existing `TelemetryPoint`s keyed by
metric prefix (`iface/`, `sockets/`, `neighbor/`, `wireguard/`).

## 3. netring device view (`view/specialized/netring.rs`)

A sensor's wire-view panel:

- **Top talkers / flows** — table of recent flows (`flow/<src>-<dst>/bytes`,
  duration, history), sortable by bytes; reuse the netflow view's table idiom.
- **Per-app bandwidth** — bars from `bandwidth/<app>/{rx,tx}_bps`.
- **L7** — DNS (rtt, unanswered %, top servers), HTTP (status-class mix per host),
  TLS (top SNIs, JA4 inventory).
- **Capture health** — drop_rate gauge, active_flows, handler/backend errors;
  red banner when drop_rate is high (the sensor is blind).
- **Detectors panel** — active detectors + counts; enable/disable/tune buttons
  driving the `detectors` command topic (Plan 08 §7).

## 4. Security view (cross-cutting, anomaly-focused)

Anomalies (Pillar A) deserve a dedicated lens beyond the generic Alerts list. Add
`CurrentView::Security` + sidebar entry:

- **Live anomaly feed** — `external` alerts where `kind == Anomaly`, grouped by
  `rule` (port scan / beacon / DGA / TLS downgrade), newest first, with
  source/detail labels and severity.
- **By-source rollup** — offending IPs ranked by anomaly count (from alert
  labels), each expandable to its alerts; a "view flows" link cross-links to the
  netring device view filtered to that IP.
- **Capture-health row** — any `SensorHealth` alerts ("sensor dropping packets").

This is ZenSight's first security surface; it reuses the `external` alert map from
Plan 07 (no new subscription).

## 5. Navigation & state

- `CurrentView`: add `Security` (and `Expectations` from Plan 08). Persist like
  existing views (`#[serde(skip)]` where appropriate).
- New state structs: `NetringViewState` (selected sort/filter, fetched detector
  status), reuse `ExpectationsState` (Plan 08) for the netlink expectations strip.
- Sidebar: add Security + Expectations entries with badges (anomaly count,
  firing-expectation count).

## 6. Icons & exhaustiveness

- Add `netlink`/`netring` SVGs in `view/icons/` and arms in
  `icons::protocol_icon::<M>(Protocol, _)`.
- Fix every exhaustive `match Protocol` (grep `Protocol::Sysinfo`): protocol
  color, label, alert-rule protocol dropdown, settings filters.

## 7. Demo mode
Extend `mock` so `--demo` populates netlink (interfaces, sockets, a firing
expectation) and netring (flows, a port-scan anomaly) so all three views render
without real sensors.

## 8. Tests (iced `Simulator`)
- Each view builds from a populated `DeviceDetailState` (synthetic metrics) and
  renders without panic; key text present.
- Security view groups anomalies by rule and counts by source.
- Detector enable/disable buttons emit the right command `Message`.
- Navigation: selecting a netlink/netring device routes to the specialized view.

## 9. Acceptance criteria
- A netlink device opens to the interfaces/sockets panel, not the generic grid.
- A netring device opens to flows/L7/capture-health with a working detector
  panel (buttons push detector commands via Plan 08).
- The Security view lists live anomalies grouped by type and by source IP.
- New protocols have icons; no non-exhaustive-match build errors.
- Simulator tests green; `zensight` test count updated.
