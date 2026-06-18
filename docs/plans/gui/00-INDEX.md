# ZenSight GUI Overhaul — Plan Index

A deep audit of the ZenSight desktop GUI (Iced 0.14) produced four bodies of
evidence: an architecture/feature map, a functional bug hunt, a UI/UX visual
audit, and web research on Iced 0.14 idioms + observability-dashboard UX +
accessibility. This index sequences the resulting work.

> Scope: the `zensight` crate (frontend) only. Sensors/exporters are out of scope
> except where the GUI consumes their data.

## The five plans

| # | Plan | Theme | Risk | Impact |
|---|------|-------|------|--------|
| [01](01-bugs-and-correctness.md) | Bugs & correctness | Fix crashes, dead state, stale-data leaks | low | high |
| [02](02-app-shell-and-navigation.md) | App shell & navigation | Persistent nav rail + top bar + breadcrumb + Back everywhere | med | high |
| [03](03-design-system.md) | Design system & theming | Type/spacing/color tokens, component primitives, kill hardcoded colors | med | high |
| [04](04-features-and-stubs.md) | Features & stubs | Finish half-built features, add high-value new ones | med | med |
| [05](05-states-feedback-accessibility.md) | States, feedback, a11y | Loading/empty/error states, hover/disabled feedback, WCAG | low–med | med |

## Recommended sequencing

```
Phase A (correctness, ship first):   01  ───────────────┐
Phase B (foundation):                03 (tokens+kit) ───┤
Phase C (structure):                 02 (shell uses 03) ┤
Phase D (polish, depends on 03+02):  05 ────────────────┤
Phase E (value, ongoing):            04 ────────────────┘
```

01 first — it's pure correctness and unblocks confident refactoring. 03 (the
design system) is the foundation 02/05 build on, so land the **tokens + component
kit** early even if individual views adopt them incrementally. 02 (app shell) is
the single highest-impact UX change and should consume 03's primitives. 04 and 05
proceed in parallel afterward, view by view.

## Guiding principles (from research)

- **Elm architecture, message-driven** — all async work through `Task`/
  `Subscription`, no shared-mutex mutation. (Sniffnet's "rethink over workaround".)
- **Two color systems, kept separate** — *semantic* status/severity (color **+**
  icon **+** label, WCAG-verified) vs *categorical* chart series (Okabe-Ito
  colorblind-safe palette).
- **Aggregate-first, drill-down second** — summary tiles with sparkline+trend up
  top; detail on demand. RED (services/flows) + USE (hosts) framing.
- **Blank ≠ healthy** — every live panel needs explicit empty / loading / error
  states distinct from "value is zero".
- **Never trap the user** — persistent nav + Back + breadcrumb on every screen.

## Cross-cutting facts established by the audit

- Navigation: the global Back-button gap on specialized device views is **already
  fixed** (commit `636c41c`, `with_device_nav`). Remaining nav work is the
  persistent shell (Plan 02).
- Dead state: `known_sensors` and `correlations` are written but never read
  (app.rs:111,113). `sensor_health` **is** read (dashboard health bar,
  app.rs:1265) — not dead.
- Stubs: `view/overview/mod.rs:117-120` (OPC-UA/netlink/netring overviews),
  `ErrorReportReceived` has no UI, OPC-UA has no specialized view.
- Worst confirmed bug: `view/chart.rs` divides by `value_range`/`time_range`
  without guarding zero (1127-1129, 1444-1446) — a flat/constant metric yields
  NaN coordinates. The threshold path (1372) already guards; the series paths
  don't.

## Verification discipline

Each task lists acceptance criteria. UI changes get an `iced_test::simulator`
assertion where feasible; async fetch/decode paths get a tokio-multi-thread test
against a real in-process Zenoh queryable (the pattern proven in
`netlink_detail.rs`). Run `cargo test -p zensight` + `cargo clippy -p zensight`
before each commit. Keep new code clippy-clean; the workspace has pre-existing
warnings in topology/chart that are not ours.
