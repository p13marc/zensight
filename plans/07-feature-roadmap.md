# Plan 07: Feature Roadmap

**Priority:** Low (enhancements after bug fixes)
**Estimated effort:** Varies per feature
**Risk:** Low (additive features)

---

## Objective

Plan the implementation of high-value features identified in the analysis. These are ordered by impact and dependency.

---

## Phase 1: Foundation Features (after Plans 01-06)

### Feature 1: Metric Persistence (Local Storage)

**Value:** High | **Effort:** Medium (3-5 days)
**Ref:** Analysis 7.1

#### Problem

All telemetry data is in-memory only. Restarting the frontend loses all history. Charts can only show data since the app was opened.

#### Design

1. **Storage backend:** SQLite via `rusqlite` (lightweight, embedded, no server needed)
2. **Schema:**

```sql
CREATE TABLE telemetry (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    protocol TEXT NOT NULL,
    source TEXT NOT NULL,
    metric TEXT NOT NULL,
    value_type TEXT NOT NULL,  -- 'counter', 'gauge', 'text', 'boolean'
    value_numeric REAL,
    value_text TEXT,
    labels TEXT,  -- JSON
    INDEX idx_source_metric (source, metric, timestamp)
);
```

3. **Write path:** Background task batches incoming telemetry (every 1s or 100 points)
4. **Read path:** Chart queries load from DB when scrolling beyond in-memory window
5. **Retention:** Configurable (default: 7 days), with hourly downsampling after 24h
6. **Location:** `~/.local/share/zensight/telemetry.db`

#### Implementation Steps

1. Add `rusqlite` dependency to `zensight`
2. Create `storage.rs` module with `TelemetryStore` struct
3. Add background write task (channel-based)
4. Integrate with chart's time window navigation
5. Add "Clear history" option in settings
6. Add retention configuration in settings

---

### Feature 2: Dashboard Layouts

**Value:** High | **Effort:** Medium (2-3 days)
**Ref:** Analysis 7.1

#### Design

1. Users can pin devices to a custom layout
2. Layouts saved as JSON in `~/.config/zensight/layouts/`
3. Support named layouts (e.g., "Production", "Lab", "Debugging")
4. Layout includes: pinned devices, sort order, expanded/collapsed sections, selected protocol tab

#### Data Model

```rust
#[derive(Serialize, Deserialize)]
pub struct DashboardLayout {
    pub name: String,
    pub pinned_devices: Vec<DeviceId>,
    pub sort_order: SortOrder,
    pub overview_expanded: bool,
    pub selected_protocol: Option<Protocol>,
    pub groups_expanded: bool,
}
```

#### Implementation Steps

1. Add `DashboardLayout` to settings/state
2. Add "Save Layout" / "Load Layout" buttons in dashboard header
3. Add layout picker dropdown
4. Persist to disk on save
5. Auto-load last used layout on startup

---

### Feature 3: Alert Forwarding (Webhooks)

**Value:** High | **Effort:** Medium (2-3 days)
**Ref:** Analysis 7.1

#### Design

When an alert triggers, send a notification to external systems via webhooks.

#### Configuration

```json5
{
  alerts: {
    webhooks: [
      {
        url: "https://hooks.slack.com/services/...",
        events: ["triggered", "resolved"],
        format: "slack",  // or "generic", "pagerduty"
      }
    ]
  }
}
```

#### Implementation Steps

1. Add `reqwest` dependency (HTTP client)
2. Create `alerts/webhook.rs` module
3. Define webhook payload formats (Slack, generic JSON, PagerDuty)
4. Fire webhook on alert trigger/resolve in `app.rs` alert handler
5. Add webhook configuration in settings UI
6. Add retry logic (3 attempts, exponential backoff)

---

## Phase 2: Advanced Features

### Feature 4: Multi-Instance Device Correlation

**Value:** High | **Effort:** Medium-High (3-5 days)
**Ref:** Analysis 7.1

#### Design

Cross-reference the same physical device seen by multiple bridges. The `CorrelationRegistry` in `zensight-bridge-framework` already provides the data model. The frontend needs to consume and display it.

#### Implementation Steps

1. Subscribe to `zensight/_meta/correlation/*` in the frontend
2. Build a `CorrelationMap` linking DeviceIds across protocols
3. Show "Also seen as" links in device detail view
4. Add a "Correlated Devices" panel showing all bridges reporting the same IP/hostname
5. Allow navigating between correlated device views

---

### Feature 5: Anomaly Detection

**Value:** High | **Effort:** High (5-7 days)
**Ref:** Analysis 7.2

#### Design

Statistical anomaly detection on metric streams using simple algorithms that run in the frontend.

#### Algorithms

1. **Z-score:** Flag values > 3 standard deviations from the rolling mean
2. **Moving average deviation:** Flag when current value deviates > X% from the N-point moving average
3. **Rate of change:** Flag sudden spikes/drops in counter rates

#### Implementation Steps

1. Create `analytics.rs` module with trait `AnomalyDetector`
2. Implement Z-score detector (requires maintaining running mean + stddev)
3. Integrate with alert system (auto-generate alert when anomaly detected)
4. Add "Enable anomaly detection" toggle per device/metric in settings
5. Show anomaly markers on charts (red dots/triangles)

---

### Feature 6: Playback Mode

**Value:** High | **Effort:** High (5-7 days)
**Ref:** Analysis 7.2

#### Design

Record telemetry sessions to disk and replay them. Requires metric persistence (Feature 1).

#### Implementation Steps

1. Add "Record" button that marks a session start/end timestamp
2. Store session metadata (name, time range, device count)
3. Add "Playback" mode that replays from DB at configurable speed (1x, 2x, 10x)
4. Show playback controls (play, pause, seek, speed)
5. Allow exporting sessions as standalone files for sharing

---

## Phase 3: Nice-to-Have Features

| Feature | Effort | Notes |
|---------|--------|-------|
| Keyboard shortcuts (vim-style) | 1-2 days | `j/k` navigation, `/` search, `Esc` back |
| Metric annotations | 2 days | Click chart to add note at timestamp |
| Threshold templates | 1 day | Predefined alert rules per protocol |
| Bulk device actions | 2 days | Multi-select for export/compare/group |
| Syslog live tail | 2-3 days | Streaming log view with regex filtering |
| SNMP MIB browser | 3-5 days | OID tree with description lookup |
| NetFlow geolocation | 3-5 days | MaxMind GeoIP + map rendering |
| Dark/light theme scheduling | 0.5 days | Time-based auto-switch |

---

## Dependency Graph

```
Plan 01 (Critical Bugs)
    |
    v
Plan 02 (Data Integrity) ---> Plan 03 (Framework)
    |                              |
    v                              v
Plan 04 (Exporters)           Plan 05 (Bridges)
    |                              |
    +----------+-------------------+
               |
               v
Plan 06 (Frontend Perf/UX)
               |
               v
Feature 1 (Persistence)
    |
    +---> Feature 2 (Layouts)
    |
    +---> Feature 3 (Webhooks)
    |
    +---> Feature 4 (Correlation)
    |
    +---> Feature 5 (Anomaly Detection)
    |
    +---> Feature 6 (Playback) [requires Feature 1]
```

## Success Criteria

Features are individually scoped. Each should:
- [ ] Have its own integration test(s)
- [ ] Include configuration documentation
- [ ] Not break existing functionality
- [ ] Pass full workspace test suite
