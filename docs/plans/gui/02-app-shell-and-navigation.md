# Plan 02 (GUI) — App Shell & Navigation

The single highest-impact UX change. Today every screen rolls its own header,
there's no persistent navigation, connection status only shows on the dashboard,
and deep paths (Topology → node → Device) have no breadcrumb. Research is
unanimous: a multi-section monitoring desktop app wants a **persistent left nav
rail + top bar**, with **breadcrumb _and_ Back** for drill-downs, and global
status always visible (Sniffnet, NN/g, Material).

> Depends on Plan 03's tokens + component kit (use them; don't hardcode).
> The per-screen Back button on device views is already fixed (`with_device_nav`,
> commit `636c41c`); this plan makes navigation *structural* rather than per-view.

---

## Target structure

```
┌──────────────────────────────────────────────────────────┐
│ TOP BAR: ZenSight · breadcrumb ········· search · conn●  │  persistent
├────────┬─────────────────────────────────────────────────┤
│ NAV    │                                                  │
│ RAIL   │   CONTENT (the current page)                     │
│ ▣ Dash │                                                  │
│ ▣ Topo │                                                  │  rail + topbar
│ ▣ Alrt │                                                  │  render once in
│ ▣ Sec  │                                                  │  app.view(),
│ ▣ Exp  │                                                  │  wrap every page
│ ▣ Sens │                                                  │
│ ───    │                                                  │
│ ⚙ Set  │                                                  │
└────────┴─────────────────────────────────────────────────┘
```

The rail and top bar are rendered **once** in `app.view()` and wrap the matched
page `Element`, replacing the ad-hoc per-view headers.

---

## S1. The shell scaffold

**New:** `view/shell.rs` exposing
`shell(nav: NavContext, top: TopBar, content: Element) -> Element`.

- `app.view()` builds the `content` from `current_view` (as today), then wraps it:
  `row![nav_rail(...), column![top_bar(...), content]]`.
- The groups sidebar + toast overlay stay layered on top (keep the existing
  `stack!`).
- Rail item = icon + label; the active item is highlighted (primary surface
  token). Each emits the existing `Open*` message — **no new navigation messages
  needed**, the rail just centralizes the affordances already scattered in
  `dashboard.rs:426-446`.

**Acceptance:** every `CurrentView` renders inside the rail+topbar; clicking a rail
item navigates; the active item is visually marked (simulator test).

---

## S2. Top bar: breadcrumb + global status + search

**Breadcrumb** (`Home > Section > Item`): derive from `current_view` +
`selected_device` + topology selection. Each ancestor segment is clickable and
emits the matching `Open*`/`ClearSelection` message. This is the cure for
"deep-path disorientation"; it supplements (does not replace) Back.

**Connection status** — move the dashboard's connection indicator (`dashboard.rs:
369-393`) into the top bar so it's visible on **every** screen. States:
Connected (success + check), Scouting/Connecting (warning + spinner — animate),
Disconnected (danger + x). Use tokens, not the hardcoded amber at
`dashboard.rs:382`.

**Global search** — promote the dashboard device-search into the top bar as a
global "jump to device/metric" (optional; can stay dashboard-local in v1). At
minimum keep a single search affordance location.

**Acceptance:** connection state is visible from Alerts/Topology/Settings (not just
Dashboard); breadcrumb segments navigate; simulator asserts breadcrumb text for a
drilled-in device.

---

## S3. Back & escape consistency

- Keep `EscapePressed` (`app.rs:1171`) but add a visible **Back** affordance to the
  top bar for every non-root page (mouse users shouldn't need the keyboard).
- Define the nav stack explicitly: pages are a shallow stack
  (`Dashboard` is root; `Device`, `Settings`, `Alerts`, etc. push). Back pops one
  level; breadcrumb jumps to any ancestor. Replace the current ad-hoc
  `previous_view` bookkeeping (`Open/Close*` save/restore pairs) with one
  `Vec<CurrentView>` nav stack in state.
- Eliminate the "two-Esc" dead-ends the audit flagged (Device+chart, Topology+node):
  Back/Esc should do the obvious single thing and never leave the user stuck.

**Acceptance:** from any page, a single Back returns one level; no screen is a
mouse-only dead-end; nav-stack unit-tested (push/pop/jump).

---

## S4. Responsive collapse

Use Iced's `responsive(|size| …)` so the rail collapses to icons-only below a
width threshold and the groups sidebar overlays rather than splits on narrow
windows. Use `Length::FillPortion` for any master/detail split (e.g. device list ↔
detail if a two-pane mode is added later).

**Acceptance:** at a narrow window the rail shows icons only and content stays
usable (manual + a `responsive` smoke test).

---

## Migration approach (incremental, low-risk)

1. Build `shell.rs` with a rail that emits existing messages; wrap `app.view()`.
2. Move connection status + breadcrumb into the top bar; delete the dashboard's
   inline connection indicator and the per-view nav button clusters as each view
   is migrated.
3. Introduce the `Vec<CurrentView>` nav stack; refactor `Open*/Close*` handlers to
   push/pop; keep message names stable to avoid churn.
4. Delete now-redundant per-view "Back to dashboard" buttons (dashboard nav
   cluster, alerts/settings/topology close buttons) once the shell covers them.

Each step compiles and ships independently. Effort: ~3–4 days. This is a broad,
visible change — land Plan 03 tokens first so the shell looks right immediately.
