# Plan 01 (GUI) — Bugs & Correctness

Fix crashes, NaN-producing math, dead state, and stale-data leaks. All
low-risk, high-value. Land this first; it unblocks confident refactoring in 02/03.

Each item: **what**, **where** (verified file:line), **why**, **fix**,
**acceptance**.

---

## B1. Chart division-by-zero → NaN coordinates (HIGH)

**Where:** `view/chart.rs` single-series draw `1127` (`time_range`) and `1129`
(`value_range`); multi-series draw `1444`/`1446`. The threshold path already
guards at `1372` (`if value_range <= 0.0 { return; }`) — the series paths do not.

**Why:** `value_range = value_max - value_min`. A **constant/flat metric** (a gauge
sitting at one value, a boolean, an idle counter) makes `value_range == 0.0`;
`time_range == 0.0` when all visible points share a timestamp (single sample, or a
1-point window). Dividing yields `inf`/`NaN`, which flows into `Point`/`Path`
geometry — blank chart, mis-render, or a canvas panic depending on backend.

**Fix:** guard both ranges once, before the draw loops, in `draw_series` and
`draw_multi_series`. When a range is zero, center the series (map every point to
0.5 of the axis) instead of dividing:
```rust
let x_frac = if time_range > 0.0 { (ts - time_start) as f64 / time_range } else { 0.5 };
let y_frac = if value_range > 0.0 { (v - value_min) / value_range } else { 0.5 };
```
Apply the same to the gridline label loop (`1180-1184`) and any sparkline
(`components/sparkline.rs`) that scales by a range.

**Acceptance:** unit test `draw`-helper math with `value_range==0` and
`time_range==0` returns finite, in-bounds fractions; a simulator test renders a
device chart for a metric with a single constant value without panicking.

---

## B2. Dead state: `known_sensors`, `correlations` (MED)

**Where:** `app.rs:111` `known_sensors`, `app.rs:113` `correlations` — inserted at
`:265`/`:269`, never read. (Note: `sensor_health` at `:109` **is** read at
`:1265` — keep it.)

**Why:** `SensorInfoReceived` and `CorrelationReceived` decode and store data that
no view consumes — wasted memory + misleading "we handle this" signal.

**Fix (choose per data):**
- `correlations` → wire into a real feature: Plan 04 F4 (correlation drill-down in
  device view / topology edges). If not adopting now, **stop storing it** (drop
  the field + handler) so the codebase doesn't imply a feature that isn't there.
- `known_sensors` → feed Plan 04 F2 (Sensors/Fleet view) or the dashboard sensor
  health bar's "registered sensors" count. Otherwise drop.

**Acceptance:** every state field is either read by a view or removed; `cargo
clippy` shows no `field is never read`.

---

## B3. `ErrorReportReceived` has no UI (MED)

**Where:** `app.rs:254-262` — logs via `tracing::warn` only; no `CurrentView` shows
sensor errors.

**Why:** sensors publish `zensight/<proto>/@/errors`; operators can't see them in
the app, so a failing sensor is invisible unless reading logs.

**Fix:** ring-buffer recent `ErrorReport`s in state (bounded, e.g. 200) and surface
them — minimal: a count badge on the sensor health bar + a panel in the Sensors
view (Plan 04 F2). Don't just log.

**Acceptance:** an injected error report appears in the UI (simulator test on the
sensors panel) and increments a visible counter.

---

## B4. Settings silently discard invalid input (MED)

**Where:** `view/settings.rs:174,176,177` — `parse().unwrap_or(default)` on user
strings; `SaveSettings` validates on save but numeric fields fall back silently.

**Why:** a typo in stale-threshold/max-history/max-alerts silently reverts to a
default with no feedback — config "doesn't stick" from the user's view.

**Fix:** parse into `Result`, keep the raw string in state, and show an inline
validation error (red border + helper text) when unparseable; disable Save while
any field is invalid. Pairs with Plan 05 (dirty-state + disabled Save).

**Acceptance:** entering `abc` in a numeric field shows an inline error and
disables Save; a valid value clears it.

---

## B5. `compute_trend` indexing without a hard guard (LOW)

**Where:** `view/device.rs:683-684` — indexes `history[len-1]`/`history[len-2]`.
Guarded at `:679` (`len < 2`) and only called where history exists, so currently
safe — but the guard and the indexing are separated and easy to break.

**Fix:** rewrite with iterator access (`last()` / `iter().nth_back(1)`) so it's
panic-proof by construction regardless of caller.

**Acceptance:** `compute_trend` on a 0- and 1-element history returns `None`/flat,
no panic; unit-tested.

---

## B6. Unbounded growth audit (LOW→MED)

**Where / why:** confirm bounds on every accumulating structure:
- device `metrics`/`history` maps — capped by `max_history`? Verify per-metric and
  per-device cap; a long-running session with churny metric names can grow the
  `HashMap` key set unboundedly even if each history is capped.
- `toasts` id counter `view/toast.rs:75` (u64, practically fine — document, no fix).
- `alerts.external` set, `sensor_health`/`known_sensors` maps — evict on
  resolve/offline?

**Fix:** add an explicit cap + eviction (LRU by last-seen) where missing; document
the bound in a comment.

**Acceptance:** a soak test (or reasoning note) shows each structure is bounded.

---

## B7. Stale fetched-detail leak across device switches (MED)

**Where:** `device.rs` `DeviceDetailState.netlink_detail` / `netring_detail` (the
on-demand tables I added). `select_device` builds a fresh `DeviceDetailState`, so
switching devices **does** clear them — verify this holds for the syslog-filter
path and any reused state. Also: the fetched detail is a point-in-time snapshot
with no "as of" timestamp, so a stale table can look live.

**Fix:** confirm fresh state on every `SelectDevice`; stamp each fetched table with
a fetch time and show "fetched 12s ago" + a refresh affordance (Plan 05 loading
states).

**Acceptance:** selecting device B never shows device A's fetched sockets/flows
(simulator test selecting two devices); the panel shows its fetch age.

---

## B8. `view_transition_key` dead field (LOW)

**Where:** `app.rs:100-101`, read only as `_transition_key` at `:1282`.

**Fix:** remove it (Iced 0.14 reactive rendering + the comment says the animation
approach doesn't work), or implement a real cross-view fade. Recommend remove now;
revisit animation under Plan 05 if desired.

---

## Sequencing & effort

B1 (the only crash-class bug) first, then B2/B8 (dead-code cleanup, trivial),
then B4/B7 (correctness with small UI), then B3/B6/B5. ~1–2 days total. Every fix
gets a unit or simulator test; run `cargo test -p zensight` + clippy per commit.
