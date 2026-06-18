# Plan 03 (GUI) — Design System & Theming

The "ugly" complaint is mostly *inconsistency*: font sizes span 9–24px with no
scale, padding/spacing are ad-hoc (0,1,2,6,8,10,15,20), ~10 files hardcode
`Color::from_rgb(...)` bypassing `theme.rs`, and there's no reusable component
kit (tables are hand-rolled `row![cell(...)]` with fixed pixel widths that overflow
on long IPs/hostnames). This plan builds the **foundation** 02 and 05 depend on.

Research basis: 3-tier tokens (primitive → semantic → component), a 5-step type
scale, an 8pt spacing scale, and two **separate** color systems — semantic
status/severity vs categorical Okabe-Ito chart series.

---

## D1. Tokens module

**New:** `view/tokens.rs` (or extend `theme.rs`).

**Type scale** (5 steps) — `const`s + a `text_size` enum:
`CAPTION=12, BODY=14, EMPHASIS=16, SECTION=20, TITLE=24`. Replace every literal
`.size(N)` across views with a token. (Audit found 11 distinct sizes in use.)

**Spacing scale** (8pt grid): `XS=4, SM=8, MD=16, LG=24, XL=32`. Replace every
`.padding(_)`/`.spacing(_)` literal. Tighten the current 10/15 mix to 8/16.

**Semantic color tokens** — name by *role*, themeable for dark/light, all
WCAG-checked:
- surfaces: `background`, `surface`, `surface_raised`, `border`, `border_subtle`
- text: `text`, `text_muted`, `text_dimmed`
- intents: `success`, `warning`, `danger`, `info`
- status: `status_online`, `status_degraded`, `status_offline`, `status_unknown`

`theme.rs` already wraps the extended palette for most of these — extend it to
cover the gaps (status, severity, card/border) and make every helper honor
`is_dark()`.

**Acceptance:** `tokens.rs` exists; a unit test asserts each
status/severity/text token pair meets WCAG contrast (≥3:1 graphic, ≥4.5:1 text)
against its surface in both themes.

---

## D2. Kill hardcoded colors

Replace every `Color::from_rgb(...)` in views with a token. Confirmed sites:
- `dashboard.rs:382` (amber "Connecting") → `warning()`
- `dashboard.rs:951-954` (device status dots) → `status_*()`
- `alerts.rs:127-129` (severity) → `severity_*()`
- `specialized/netflow.rs:228-231` (protocol colors) → `protocol_color()`
- `toast.rs:23-26` (severity) → tokens
- `components/gauge.rs:94-98`, `components/sparkline.rs:28` → tokens
- `chart.rs:900-990` (extensive duplicated colors) → tokens + Okabe-Ito (D4)
- `groups.rs:308,355-363` (group tag colors) → keep as an explicit categorical
  palette constant (group colors are user-chosen categorical, not semantic) but
  source it from one place.

Add a CI grep guard: fail if `Color::from_rgb` appears outside `tokens.rs`/
`theme.rs`/the categorical palette constant.

**Acceptance:** `grep -rn 'Color::from_rgb' view/ | grep -v tokens.rs` is empty
(except the sanctioned palette files); both themes render correctly.

---

## D3. Component primitives

**New:** `view/components/` (some exist: gauge, sparkline). Add reusable helpers,
each a `fn(...) -> Element` with a token-driven style closure (`Catalog` model):

- **`card(content)`** — `container` with `surface` bg, `border_subtle`, radius,
  padding `MD`. Replaces bare `column![...]` sections (netlink/netflow/settings
  sections are currently unboxed walls of rows).
- **`section_header(title, actions)`** — `SECTION`-size text + optional trailing
  buttons, bottom gap `SM`.
- **`data_table(columns, rows)`** — the big one. A `scrollable` + header row +
  zebra-striped body using `FillPortion` columns (not fixed px), with per-cell
  truncation + tooltip for long values. Replaces the hand-rolled fixed-width
  tables in `specialized/netlink.rs:78-110`, `netflow.rs:295-338`, the device
  metrics table, and the on-demand detail tables. Fixes the overflow-on-long-IP
  problem.
- **`badge(intent, icon, label)`** — status/severity pill: color **+** icon **+**
  text (never color alone). Used by dashboard status dots, alerts, security,
  syslog severity.
- **`empty_state(icon, message, action?)`** — centered icon + muted text + optional
  CTA. Replaces the inconsistent bare strings ("No interface data", "No flow data
  available", "No metrics received yet…", sizes 12–16, some centered some not).
- **`stat_tile(label, value, trend, status)`** — KPI tile: current value +
  sparkline + directional arrow + status color (research: "current value + trend
  together"). For dashboard/overview summary tiles.

**Acceptance:** each primitive has a simulator render test; at least one real view
(start with `specialized/netlink.rs`) is migrated to `card` + `data_table` +
`empty_state` as the reference implementation.

---

## D4. Two color systems, kept separate

- **Semantic** (status/severity/intent) — the tokens in D1, color+icon+label.
- **Categorical** (chart series, per-protocol) — the **Okabe-Ito** colorblind-safe
  palette: `#E69F00 #56B4E9 #009E73 #F0E442 #0072B2 #D55E00 #CC79A7 #000000`.
  Use for multi-series charts (`chart.rs`) and per-protocol series. Never reuse
  status red/green as a category color (red/green is the worst confusion pair).

**Acceptance:** chart series cycle the Okabe-Ito palette; a multi-series chart is
distinguishable in grayscale (manual check) and uses no `from_rgb` literals.

---

## D5. Typographic & spacing migration

Mechanical sweep: replace literal `.size()/.padding()/.spacing()` with tokens,
view by view, starting with the most-seen screens (dashboard, device, alerts).
Low risk, high consistency payoff. Do it alongside D3 migration per view to avoid
double-touching files.

**Acceptance:** no literal font sizes outside `tokens.rs`; spacing literals limited
to the 8pt set (grep guard optional).

---

## Sequencing & effort

D1 (tokens) + D2 (kill hardcoded) first — small, mechanical, unblocks everything.
D3 (component kit) next, migrating `netlink.rs` as the reference. D4 with the chart
work. D5 as an ongoing sweep folded into each view's migration. ~4–6 days for the
kit + reference migrations; the full sweep is incremental. **Land D1–D3 before
Plan 02** so the shell looks polished on arrival.
