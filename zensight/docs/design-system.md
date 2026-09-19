# Design system (D2)

The frontend draws all colors, fonts, and spacing from one design system so the
UI stays visually consistent and theme-aware. This is a hard rule: it is
**enforced by a CI color guard**, and it is the reason view modules must never
reach for ad-hoc color literals.

## The one rule: colors come from three places only

Every color must originate in one of these locations:

| Location | What it holds |
|----------|---------------|
| `src/view/theme.rs` | Theme-aware `ThemeColors` accessors **and** theme-independent `pub const` palettes. |
| `src/view/tokens.rs` | Font-size and spacing tokens only — **no colors, by design**. |
| `src/view/components/` | The shared widget kit; data colors are built with `kit::rgb` / `kit::rgba`. |

Anywhere else, a raw `Color::from_rgb(...)` / `Color::from_rgba(...)` is
forbidden.

### The CI guards

CI is **Forgejo Actions** (`.forgejo/workflows/ci.yml`); there is no `.github/`
in this repository, and this paragraph pointed at `.github/workflows/rust.yml`
until #1125 — a guard nobody could find is one nobody maintains.

Three merge-gating greps, in the `lint` job:

- **colour** — `Color::from_rgb`/`::new`/the constants/`color!`/the struct
  literal, anywhere outside `view/theme.rs`, `view/tokens.rs` and
  `view/components/`. If a view needs a new colour, add it to the palette (or
  plumb it through `ThemeColors`) rather than inlining a literal;
- **type scale** — a raw `.size(N)` anywhere outside those same three places
  (#1125). Use `view::tokens::font`;
- **spacing** — a **ratchet**, not a wall (#1125). `.padding(N)`/`.spacing(N)`
  are counted, and the count may not rise. See below for why that one is
  different.

### Why spacing ratchets instead of failing

The type sweep was a rename: 598 of the 662 `.size(N)` calls were *already* on
a scale step, and the rest were one view's private 13/18/22 spelling of
body/section/title. Zero pixels moved for 90 % of them.

Spacing is not like that. `.spacing(10)` appears 102 times and both `SM` (8)
and `MD` (16) are defensible readings of it; `.spacing(6)` appears 59 times
between `XS` and `SM`. Choosing, 350 times, is a **layout change** — the kind
that wants somebody looking at the window, not a regex. So the count in
`zensight/src/view/.spacing-ratchet` may only go down: sweep a file, lower the
number, and it cannot come back. CI fails in **both** directions, so a sweep
that forgets to lower the ceiling is caught too.

## `theme.rs` — colors

Two kinds of color live here:

- **`ThemeColors<'a>`** — a thin wrapper over an Iced `Theme` that exposes
  semantic, *theme-aware* accessors: `background()`, `background_weak/strong/
  strongest()`, `text()`, `text_muted()`, `text_dimmed()`, `success()`,
  `danger()`, `warning()`, `primary()`, `secondary()`, `border()`,
  `border_subtle()`, plus chart- and topology-specific colors
  (`chart_background()`, `chart_grid()`, `chart_highlight()`, …). Each accessor
  returns the right shade for the active light/dark theme. Construct one with
  `ThemeColors::new(&theme)` and read colors from it instead of hardcoding.

- **Theme-independent `pub const` palettes** — fixed colors whose meaning does
  not change between light and dark, grouped by domain:
  - **Device status**: `STATUS_ONLINE`, `STATUS_DEGRADED`, `STATUS_OFFLINE`,
    `STATUS_UNKNOWN` (the 4-color device status model: green / orange / red / gray).
  - **Alert severity**: `SEVERITY_INFO`, `SEVERITY_WARNING`, `SEVERITY_CRITICAL`.
  - **Syslog levels**: `SYSLOG_EMERGENCY`, `SYSLOG_ERROR`, `SYSLOG_WARNING`,
    `SYSLOG_NOTICE`, `SYSLOG_INFO`, `SYSLOG_DEBUG`.
  - **Toasts**: `TOAST_INFO`, `TOAST_SUCCESS`, `TOAST_WARNING`, `TOAST_ERROR`.
  - **Accents**: `ACCENT_GOLD`, `ACCENT_STALE`, `ACCENT_ANOMALY`, and the
    `PROTOCOL_CATEGORY` palette.

These constants are the *only* sanctioned `Color::from_rgb` call sites, because
they live in `theme.rs`.

## `tokens.rs` — type & spacing

`tokens.rs` holds the *dimensional* tokens — no colors. Use these instead of bare
`.size(13)` / `.padding(10)` / `.spacing(15)` calls so every view draws from one
scale.

**Type scale** (`font`, seven steps, pixels as `f32`):

| Token | px | Use |
|-------|----|-----|
| `MICRO` | 10 | Superscripts, unit suffixes, axis ticks. The floor — below this, text stops being legible at 100 % scaling. |
| `DENSE` | 11 | Dense table cells and per-row metadata. |
| `CAPTION` | 12 | Captions, labels, metadata. |
| `BODY` | 14 | Default body text. |
| `EMPHASIS` | 16 | Emphasis, card titles, key values. |
| `SECTION` | 20 | Section headers within a page. |
| `TITLE` | 24 | Page title (one per screen). |

**Spacing scale** (`Spacing`, on an 8pt grid, pixels as `f32`):

| Token | px | Use |
|-------|----|-----|
| `XS` | 4 | Tight icon↔label gap (the only sub-8 value; use sparingly). |
| `SM` | 8 | Default gap between related elements. |
| `MD` | 16 | Gap between groups / card inner padding. |
| `LG` | 24 | Gap between sections. |
| `XL` | 32 | Page-level padding / large separations. |

`MICRO` and `DENSE` are new in #1125, and they are a **finding**, not a
loosening. `.size(9)`/`.size(10)`/`.size(11)` appeared **281 times**,
concentrated in exactly the views where a 12 px cell does not fit —
`specialized/sysinfo.rs`, `specialized/syslog.rs`, `device.rs`. That is not
drift from the scale; it is a requirement the five-step scale did not have. The
alternative was resizing 281 dense cells up to `CAPTION`, which is a layout
change made silently. They are named so the guard can enforce something true.

Assertions in `tokens.rs` guard the ordering of these constants, so the scale
can't be silently reordered.

## `components/` — the widget kit

`src/view/components/` is the shared widget kit (`tabs.rs`, `data_table.rs`,
`gauge.rs`, `sparkline.rs`, `progress_bar.rs`, `status_led.rs`, and `kit.rs`).
Because it lives under `view/components/`, it is allowed to construct colors —
data-driven series colors are built with the helpers in `kit.rs`:

```rust
kit::rgb((0.40, 0.75, 0.45))        // opaque
kit::rgba((0.40, 0.75, 0.45), 0.5)  // with alpha
```

Prefer building shared widgets here (and reusing them from views) over
hand-rolling styled widgets in a view module — that keeps both the look and the
color-guard compliance in one place.

### The verdict chip (`verdict.rs`, #791)

`verdict_badge(&Verdict)` renders the three-state payload verdict on
`kit::badge` (dot + words — meaning never by colour alone). Colour resolves
through the verdict's *pole*, never a boolean:

| Verdict | Swatch |
|---|---|
| `Valid` | `STATUS_ONLINE` |
| `Invalid` | `STATUS_OFFLINE` |
| `NotValidated(FeatureOff \| NoRegistry)` — *chose not to* | `STATUS_UNKNOWN` |
| `NotValidated(NoSchema \| KindUnsupported \| Undecodable \| BadSchema)` — *could not* | `JUDGEMENT_UNOBSERVABLE` |

The rule (the whole point of #791): **`NotValidated` must never read as a
passing check** — same failure #746 removed from the fleet view. Six reasons,
two groups; the full reason always rides the label. A property test in
`verdict.rs` pins "never green"; don't add a verdict rendering anywhere else
without going through this component.

## Adding a color: checklist

1. Is it theme-dependent? Add a `ThemeColors` accessor in `theme.rs`.
2. Is it a fixed semantic color (status/severity/level/accent)? Add a
   `pub const` to the right palette group in `theme.rs`.
3. Is it a data-series color inside a shared widget? Build it with `kit::rgb` /
   `kit::rgba` under `view/components/`.
4. Never inline `Color::from_rgb(...)` in a `view/*.rs` module — the CI guard
   will reject it.
