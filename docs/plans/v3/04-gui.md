# Plan v3‑04 — GUI

The overhaul shipped the shell, design system, all specialized views, fetch
states, overviews, Sensors view, **alert grouping + acknowledge**, settings
validation, metric‑threshold authoring, and on‑demand query clients. This round
targets the remaining structural gaps + research‑backed "feels live/trustworthy"
features.

> Verified against the views. Research: Netdata tiered local store, USE/RED tiles,
> Alertmanager silence/group/inhibit, Hubble flow‑derived topology, real‑time
> dashboard UX (freshness, skeletons, command palette).

---

## A. Local tiered time‑series store **[Wave 1] — foundation**

Today metric history is in‑memory `VecDeque` (max 500/metric, `device.rs:65`),
**lost on restart**; no multi‑hour/day trends. Adopt a **Netdata‑style ring‑buffer
+ tiered downsampling** local store (not a server TSDB):

- 3 tiers (e.g. per‑sec 24h / per‑min 7d / per‑hour 90d), each evicted at size or
  time; query engine picks tier by zoom range.
- Persist to `~/.local/share/zensight/metrics.{db|store}` — embedded
  (`redb`/`sled`/SQLite) or a compact append‑only ring file.
- Subscription writes through; charts read from the store so trends survive
  restart and a device view opens pre‑populated.

**Why:** every history/trend/sparkline feature depends on this; restart‑survival is
the difference between "toy" and "tool". Effort: **L** (the keystone of this plan).

## B. Honest freshness / liveness **[Wave 1] — trust foundation**

- Per‑metric **"as of HH:MM:SS" / "5s ago"** age label, fading to muted past
  `stale_threshold` (today only a bool `is_stale`, `device.rs:47`).
- A global **Live / Stale / Paused** indicator + last‑update pulse in the top bar
  (connection status is there; add data freshness).
- **Reconnection UX:** auto‑retry with backoff + a "Reconnecting…" banner;
  skeletons (not spinners) for first load; 200–400ms transitions.
- Search/filter **"filtering…"** affordance during the 300ms debounce.

**Why:** research's #1 UX rule — prefer an honest "data as of 10:42" over fake
real‑time; trustworthy liveness underpins everything. Effort: **S–M**.

## C. Trend badges + dashboard sparklines **[Wave 2]**

- Signed **% delta + arrow** badge next to key metrics (over last hour, from the
  store) — redundant encoding (never color alone).
- 24h **sparkline** on dashboard device cards (CPU/mem/RX), not only in the detail
  chart. Reuse `components/sparkline.rs`.
- Optional cheap **"anomaly bit"** per metric (value beyond p99 of its recent
  distribution) → a ranked "what's weird now" strip. Effort: **S–M**.

## D. Real topology edges (kill the simulated mesh) **[Wave 2]**

Today `topology/mod.rs:147` builds a **demo mesh** between any active nodes. Build
edges from **observed data**:
- netlink `@/query/neighbors` (L2 adjacency) + netring `@/query/flows` (real
  src→dst with bytes/pkts) via the proven Fetch pattern.
- Render the already‑tracked `selected_edge` detail panel (src/dst/proto/bytes/
  last‑seen) — currently selected but not shown.
- Per‑node severity arc‑ring from `apply_alerts`; force‑directed layout (exists).
- Surface the dead `correlations` map as a "seen by N sensors" node label.

**Why:** Hubble's model — topology from live flow data, not config. Effort: **M**.

## E. Alerting depth: timeline · silence · notifications **[Wave 3]**

Grouping + acknowledge already shipped. Add:
- **Incident timeline** — store firing→resolved transitions per `alert_key`; show
  on incident drill‑down ("Firing 2m → Resolved → Firing 1m").
- **Silence** — "Mute 1h/4h/24h" per incident/source (Alertmanager model);
  muted incidents hidden, with a muted‑count chip.
- **Desktop notifications** (opt‑in, `notify-rust`) — gated to **critical
  transitions only** (alert‑fatigue research); everything else stays in‑app.
- Alert **detail expansion**: metric value at fire time, threshold, rule. Effort: **M**.

## F. Search · favorites · customization **[Wave 3]**

- **Global metric search** across devices ("find all `*queue*`") → results modal.
- Alert **severity/source filter** pills (data already grouped via
  `external_by_source`).
- **Metric favorites** — ⭐ pin to a "Favorites" section per device (persisted).
- Saved **filter presets** / dashboard tabs (Production/Testing). Effort: **S–M** each.

## G. Polish backlog (ship‑quality) **[Wave 3]**

- **D2 — kill hardcoded colors:** 16 `Color::from_rgb` sites outside theme/tokens
  (toast severity, alerts severity, dashboard status dots, device trend/stale,
  overview sysinfo/syslog severity). Migrate to `theme.rs` helpers + add a CI grep
  guard. Keep the intentional categorical group palette, centralized.
- **L5 — badges everywhere:** status/severity by color **alone** in ~6–8 spots
  (dashboard grid status dot, device trend arrows, syslog/overview severity).
  Apply the existing `badge` (color+icon+label).
- **WCAG contrast unit test** in `tokens.rs` (≥3:1 graphic / ≥4.5:1 text, both
  themes); fix failures.
- **Keyboard shortcuts + help overlay** (Ctrl+? / command palette): Dashboard/
  Alerts/Topology/Settings/Search.
- **Uniform empty states** — replace remaining hardcoded "No data" strings
  (`device.rs:737`, `netflow.rs:192`, …) with `empty_state`.
- **OPC‑UA specialized view** (F3 — still a generic fallback); **rule editing +
  "alert on this metric" promotion** from a device metric row (F7);
  **dedicated netlink/netring icons** (`icons/mod.rs:260`). Effort: mostly **S**.

---

## Sequencing

> Follow [Plan 05 §5](05-architecture-and-conventions.md): the local store is a
> hot in‑memory ring + **`redb`** warm/cold tiers (pure‑Rust, ACID, zero‑copy;
> chosen over sled/SQLite); `MetricId(u32)` interning for compact keys; all
> store/flush I/O off the UI thread via `Task::future` + `spawn_blocking` — never
> block Iced `update`/`view` on disk.

- **Wave 1: A (local store) + B (freshness).** A is the foundation; B is the
  trust layer — do these first; most later items depend on the store.
- **Wave 2: C (trends/sparklines) + D (real topology edges).**
- **Wave 3: E (alert depth) + F (search/favorites) + G (polish: D2/L5/contrast/
  keyboard/empty‑states/OPC‑UA/promotion).**
- Each view change gets an `iced_test::simulator` assertion; async paths get the
  in‑proc‑queryable test pattern; keep new code clippy‑clean.
