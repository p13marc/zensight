# Plan 07 — Frontend: render sensor alerts + topology enrichment

**Goal:** make the desktop app *show* sensor-pushed alerts (Pillar A + B) in the
existing Alerts view and toast system, with firing/resolved lifecycle, and lay
the groundwork for topology enrichment from netlink/netring data.

**Depends on:** 02 (Message variants + decode already added there). **Effort:** M.

Reconciles the **two `Alert` types** (see [INDEX D2/D7](00-INDEX.md)): the existing
`view::alerts::Alert` (local threshold-rule alerts) stays as the *ephemeral*
convenience; incoming `common::Alert` (sensor-pushed, *durable*) is adapted into
the same view.

> **This plan is the alert *ingest + display* layer.** It is the prerequisite for
> three follow-on GUI plans: authoring/pushing what produces alerts
> ([Plan 08](08-gui-command-channel.md)), per-protocol specialized views +
> Security view ([Plan 09](09-specialized-views.md)), and the topology alert
> overlay ([Plan 10](10-topology-enrichment.md)). The `Message::Connected(Arc<Session>)`
> change that lets the app talk back to sensors lands in Plan 08, not here.

---

## 1. Reconcile the alert types (`view/alerts.rs`)

The frontend keeps its rule engine *and* gains an external-alert list.

```rust
// Severity bridge — common is plain, frontend has iced colors.
impl From<zensight_common::AlertSeverity> for Severity {
    fn from(s: zensight_common::AlertSeverity) -> Self {
        match s { Info => Severity::Info, Warning => Severity::Warning, Critical => Severity::Critical }
    }
}

// New: a unified row the view renders, from either source.
pub enum AlertRow<'a> {
    Rule(&'a Alert),                       // existing threshold alert
    Sensor(&'a zensight_common::Alert),    // pushed by a sensor/sentinel
}
```

Extend `AlertsState`:
```rust
pub struct AlertsState {
    // ... existing fields ...
    /// Sensor-pushed alerts keyed by alert_key (lifecycle-managed).
    pub external: HashMap<String, zensight_common::Alert>,
}
impl AlertsState {
    pub fn ingest_external(&mut self, a: zensight_common::Alert) -> ExternalAlertOutcome {
        // key = a.alert_key(); on Firing insert/update, on Resolved remove.
        // return New | Updated | Resolved so app.rs can toast appropriately.
    }
    pub fn clear_external(&mut self, alert_key: &str) { self.external.remove(alert_key); }
    pub fn active_external(&self) -> impl Iterator<Item=&zensight_common::Alert>;
    pub fn unacknowledged_count(&self) -> usize; // include external firing in the badge
}
```

Render: `render_alerts_section` lists rule-alerts (unchanged) **and** an
"Anomalies & expectations" group from `external` (sorted by severity then time),
each row showing `summary`, severity color, `source`, `kind`, and key labels.
Acknowledge applies to both.

## 2. `app.rs` handling

```rust
Message::AlertReceived(alert) => {
    match self.alerts.ingest_external(alert.clone()) {
        ExternalAlertOutcome::New => self.toasts.push(
            toast_severity(alert.severity), alert.summary.clone()),
        ExternalAlertOutcome::Resolved => self.toasts.push(
            ToastSeverity::Success, format!("Resolved: {}", alert.summary)),
        ExternalAlertOutcome::Updated => {}   // no re-toast (dedup)
    }
}
Message::AlertCleared { alert_key, .. } => {
    if let Some(a) = self.alerts.external.remove(&alert_key) {
        self.toasts.push(ToastSeverity::Success, format!("Resolved: {}", a.summary));
    }
}
```

`toast_severity`: Critical→Error, Warning→Warning, Info→Info. (`decode` of
`AlertReceived`/`AlertCleared` was added in Plan 02 §5–6.)

## 3. New protocols in the UI

`Protocol::Netlink` / `Protocol::Netring` were added to common (Plan 03/05).
Frontend touch-points:
- `view/icons/`: add `netlink`/`netring` SVGs + arms in
  `icons::protocol_icon::<M>(Protocol, IconSize)`.
- Any `match Protocol { … }` that must be exhaustive (color, label, filter
  dropdowns in `view/alerts.rs` rule form, settings) — add the two arms. Grep
  `Protocol::Sysinfo` to find all exhaustive matches.
- Dashboard/device views render them like any other protocol (no special-casing
  needed — they're just `TelemetryPoint`s).

## 4. Alerts badge / navigation

- Sidebar Alerts entry shows a badge = `unacknowledged_count()` (rule + external
  firing). Critical external alerts tint it red.
- Optional: clicking an external alert that has a `source` label navigates to that
  device's detail view.

## 5. Topology enrichment → moved to [Plan 10](10-topology-enrichment.md)

The first draft sketched topology enrichment here; the review promoted it to its
own plan (the user emphasized GUI depth). This plan only ensures the **hook**
exists: `alerts.external` is stored where `TopologyState` (Plan 10) can read it to
tint nodes / draw broken edges. Nothing else topology-related lands here.

## 6. Demo mode

Extend `mock` (`zensight/src/mock.rs`) with:
- `mock::netlink::host(...)` interface counters + a sample expectation alert.
- `mock::netring::sensor(...)` flows + a sample port-scan anomaly alert.
- `--demo` periodically emits a firing alert then resolves it, so the alert
  lifecycle UI is demoable without real sensors.

## 7. Tests (iced `Simulator`)
- `ingest_external`: New/Updated/Resolved outcomes; badge count; resolved removes
  row.
- `From<AlertSeverity>` mapping.
- Simulator: build `alerts_view` with a populated `external` map; assert the
  summary text renders and an acknowledge click emits the right `Message`.
- Decode test: `@/alerts/<key>` Put → `AlertReceived`, Delete → `AlertCleared`
  (in `subscription.rs` tests).

## 7b. Future note — shared acknowledgement (deferred)

Acknowledgement is **local GUI state** today and stays that way here. For
multi-operator setups, ack could be published back (e.g.
`zensight/<protocol>/@/alerts/<key>/ack`) so every GUI and the sensor agree an
alert is handled. Out of scope (INDEX §5); leave a TODO where `acknowledge()`
mutates `external`.

## 8. Acceptance criteria
- A `common::Alert` published by a sensor appears in the Alerts view within the
  subscription latency and raises a toast; its Resolved transition removes the row
  and raises a recovery toast.
- The Alerts badge counts external firing alerts.
- `Protocol::Netlink`/`Netring` render with icons; no non-exhaustive-match build
  errors.
- Demo mode shows a full firing→resolved alert cycle.
- Simulator + unit tests green; `zensight` test count updated in `CLAUDE.md`.
