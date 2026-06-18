# Plan 05 (GUI) — States, Feedback & Accessibility

Polish that makes the app feel responsive and trustworthy: explicit
loading/empty/error states (blank ≠ healthy), real interaction feedback
(hover/pressed/disabled, async-in-flight), and accessibility (color+icon+label,
WCAG contrast). Depends on Plan 03's component kit (`empty_state`, `badge`,
spinner).

---

## L1. Async fetch states (loading / error)

**Where:** on-demand detail fetches have no in-flight or error state —
`netlink_detail.rs` / `netring_detail.rs` hold `Option<Vec<T>>`, so the user clicks
"Fetch" and sees nothing until data arrives (or forever, on failure).

**Fix:** model fetch as a state machine, not an `Option`:
```rust
enum Fetch<T> { Idle, Loading, Ready { data: T, at: i64 }, Error(String) }
```
Render: `Idle` → just the button; `Loading` → spinner + disabled button; `Ready` →
table + "fetched Ns ago" + refresh; `Error` → inline error + retry. The
`query_*` Tasks already return a failure message (`CommandFeedback{success:false}`)
— route it into `Error` instead of only a toast.

**Acceptance:** clicking Fetch shows a spinner and disables the button; a failed
fetch shows an inline retry; a stale table shows its age. Simulator tests for each
state.

---

## L2. Empty states everywhere

**Where:** bare strings of varying size/alignment — `netlink.rs:71`,
`netflow.rs:192`, `syslog.rs:548`, `device.rs:737`, `groups.rs:515`.

**Fix:** replace all with the `empty_state(icon, message, action?)` primitive
(Plan 03 D3). Distinguish "no data yet" (waiting on first sample — the **Unknown**
status) from "filtered to empty" (offer a clear-filter action) from "feature
needs setup".

**Acceptance:** every list/table has a consistent empty state; "no data yet" reads
differently from "no match".

---

## L3. Interaction feedback

**Where:** clickable cards/rows (`dashboard.rs:1038`, `security.rs:82`,
`alerts.rs:750`) have no hover; buttons lack disabled styling when forms are
incomplete (`alerts.rs` Add/Test, `settings.rs` Save).

**Fix:**
- hover style on clickable containers (`surface_raised` token) so cards *look*
  clickable.
- disabled styling (muted, no hover) when an action isn't available; drive Save/Add
  disabled-state off form validity (ties to B4 + F8 dirty-state).
- pressed/active feedback via the built-in button catalog states.

**Acceptance:** cards lighten on hover; Add/Save are visibly disabled until valid;
manual + simulator (disabled button emits no message on click).

---

## L4. Persistent connection + activity feedback

**Where:** connection status only on the dashboard (`dashboard.rs:369`).

**Fix:** moved to the top bar by Plan 02 S2 — here, make it *animated/clear*:
Scouting/Connecting shows a spinner (not a static red dot + "Connecting" in
hardcoded amber); Disconnected offers a "retry/settings" affordance. Add a subtle
"last update" pulse so a frozen feed is distinguishable from a quiet one.

**Acceptance:** connection state is unambiguous and visible on every screen;
"connecting" animates.

---

## L5. Status & severity: color + icon + label (a11y)

**Where:** statuses/severities are often color-only (dashboard dots in grid view;
alert severity as a left border; security count badges).

**Fix:** every status/severity uses the `badge` primitive = color **+** icon **+**
text. Device status: dot icon + "Online/Degraded/Offline/Unknown". Alert severity:
glyph + "Info/Warning/Critical". Never red/green as the sole distinction. Add a
small legend (hover/help) explaining the semantics.

**Acceptance:** no status/severity is conveyed by color alone (audit checklist);
grayscale screenshot still distinguishes statuses.

---

## L6. WCAG contrast pass

**Where:** several hardcoded colors fail in one theme (audit flagged light-theme
readability).

**Fix:** after Plan 03 D1/D2 centralize tokens, run a contrast check (unit test in
D1) and adjust token values to meet ≥3:1 (graphic/UI) and ≥4.5:1 (text) in **both**
themes. Verify chart strokes (Okabe-Ito) clear 3:1 on the chart background.

**Acceptance:** the D1 contrast test passes for every token pair in both themes;
manual spot-check of charts/badges.

---

## L7. Search/debounce feedback

**Where:** dashboard + device metric search debounce (~300ms) with no indication —
reads as lag. (Note: the device filter is functional — debounced via the tick and
applied on `metric_filter`, `device.rs:339` — this is purely a feedback gap, not a
bug.)

**Fix:** subtle "filtering…" affordance or input-border tint during the debounce
window; show "X of Y" result counts (device view already does — extend to
dashboard).

**Acceptance:** typing shows a brief filtering indicator; result counts update.

---

## L8. Settings dirty-state

**Where:** `settings.rs` Save/Reset always enabled; no unsaved-changes cue.

**Fix:** dirty flag (current form ≠ persisted); show "Settings •" + enable Save only
when dirty; confirm-on-leave if dirty. Pairs with B4 (validation) + F8.

**Acceptance:** changing a field marks dirty + enables Save; saving clears it.

---

## Sequencing & effort

L1 + L2 (states) first — they need the Plan 03 kit and have the biggest "feels
broken" payoff. L5 + L6 (a11y) alongside the token work. L3/L4/L7/L8 as polish.
~3–4 days, incremental and view-by-view. Mostly simulator-testable; the contrast
test is a pure unit test.
