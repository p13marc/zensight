# Plan 04 (GUI) — Features & Stubs

Finish half-built features and add high-value new ones. Ordered by value/effort.
Each consumes data the sensors *already publish* unless noted.

---

## Finish existing stubs

### F1. Protocol overviews (netlink / netring / OPC-UA)
**Where:** `view/overview/mod.rs:117-120` render literal "… overview not
implemented". **Fix:** implement aggregations matching the others — netlink: host
count, interfaces up/total, sockets by state, default-route present; netring:
flows/sec, active flows, top talkers, TCP resets; OPC-UA: node/session counts.
Use the `stat_tile` primitive (Plan 03 D3). **Acceptance:** each protocol tab shows
real aggregates (simulator test with mock data).

### F2. Sensors / Fleet view (NEW `CurrentView::Sensors`)
**Consumes:** the already-stored `sensor_health` (app.rs:1265 feeds the dashboard
bar) + `known_sensors` (currently dead, B2) + `ErrorReportReceived` (currently
log-only, B3). **Build:** a page listing every sensor with health
(healthy/degraded/unhealthy badge), device counts (total/responding/failed), last
poll duration, recent error count + a drill-in to the error ring-buffer. This
turns three dead/log-only data streams into one operator-grade view.
**Acceptance:** sensors render with health badges; an injected error report appears
in the sensor's error list.

### F3. OPC-UA specialized device view
**Where:** `specialized/mod.rs` returns `None` for OPC-UA → generic fallback.
**Fix:** a tailored view (nodes, sessions, subscription status) like the other
protocols. **Acceptance:** an OPC-UA device shows the specialized view, not the
generic table.

### F4. Correlation drill-down
**Consumes:** the dead `correlations` map (B2) + `_meta/correlation/*`. **Build:**
show cross-sensor device identity ("this host is seen by snmp+sysinfo+netlink") in
the device header, and use neighbor adjacency (netlink `@/query/neighbors`) +
netring flows to draw **real** topology edges instead of the current simulated
mesh. **Acceptance:** a device correlated across sensors shows its sources; at
least one real (non-simulated) topology edge renders.

### F5. Dedicated netlink / netring icons
**Where:** `view/icons/mod.rs:260` TODO — both fall back to the generic icon. Add
SVGs (netlink: NIC/interface glyph; netring: flow/ring glyph). Low effort, visible
polish.

---

## New high-value features

### F6. Alert grouping, acknowledge, and dedup UX
**Where:** `view/alerts.rs` is a flat list; `view/security.rs` groups by source.
**Build (research-backed):** group the alerts/anomalies feed by source (and/or
rule) into collapsible incidents (the sensor already emits a stable `alert_key` =
source+rule+labels FNV-1a — that *is* the dedup key); show severity as
color+icon+label badges; add acknowledge per-incident; tier display by severity.
Add filter-by-protocol/device. **Acceptance:** N alerts from one source collapse to
one incident with a count; acknowledge marks the incident; simulator test.

### F7. Rule & expectation editing
**Where:** alerts rules and netlink expectations support create+delete but not
**edit**. **Fix:** an edit affordance that repopulates the form. Also surface
netlink expectations *inline in the netlink device view* (not only the global
Expectations page), with the metric-threshold expectation (the keystone) authorable
from a metric row ("alert me when this metric > X" → pushes a `metric` expectation
via the command channel — the GUI-rule-promotion path). **Acceptance:** editing a
rule updates it in place; a metric row can promote a threshold to a headless
expectation (verified live against the sensor command channel).

### F8. Settings: connectivity test + live-apply awareness
**Where:** `view/settings.rs` — Zenoh settings require restart, no test button.
**Build:** a "Test connection" button that attempts a scout/connect and reports
success/latency; clearly mark which settings apply live vs need restart; dirty-state
indicator (Plan 05). **Acceptance:** test button reports reachable/unreachable.

### F9. Dashboard: sorting, bulk group assignment, density toggle
**Where:** `view/dashboard.rs` — no client-side sort, no multi-select.
**Build:** sort by name/status/last-seen/protocol; multi-select → assign group in
one action (groups already support per-device assignment); a compact/comfortable
density toggle. **Acceptance:** sort reorders; multi-select assigns a group to N
devices at once.

### F10. Metric favorites / pinning + export
**Build:** pin key metrics to the top of a device view; persist pins; export the
device's metric table (CSV/JSON already exist for the device — extend to the
filtered/selected set). Low effort, daily-use value.

---

## Sequencing & effort

Finish-stubs first (F1, F2, F5 — they pay back dead state/log-only data and are
mostly assembly): ~2–3 days. Then F6 (alerts UX — high operator value) and F7
(editing + metric-promotion): ~3 days. F3/F4/F8/F9/F10 as capacity allows. Every
feature that talks to a sensor (F7 promotion, F8 test) gets a live verification
against a real in-proc Zenoh peer; UI-only features get simulator tests.
