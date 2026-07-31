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

### The CI color guard

CI (`.github/workflows/rust.yml`) runs a merge-gating grep that fails the build
if `Color::from_rgb` appears outside `view/theme.rs`, `view/tokens.rs`, and
`view/components/`. So if a view needs a new color, add it to the palette (or
plumb it through `ThemeColors`) rather than inlining a literal.

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

**Type scale** (`FontSize`, five steps, pixels as `f32`):

| Token | px | Use |
|-------|----|-----|
| `CAPTION` | 12 | Captions, labels, dense table cells, metadata. |
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

Compile-time assertions in `tokens.rs` guard the ordering of these constants, so
the scale can't be silently reordered.

## `components/` — the widget kit

`src/view/components/` is the shared widget kit (`tabs.rs`, `data_table.rs`,
`gauge.rs`, `sparkline.rs`, `progress_bar.rs`, `status_led.rs`, and `kit.rs`).
Because it lives under `view/components/`, it is allowed to construct colors —
data-driven series colors are built with the helpers in `kit.rs`:

```rust
kit::rgb((0.40, 0.75, 0.45))        // opaque
kit::rgba((0.40, 0.75, 0.45), 0.5)  // with alpha
```

`kit.rs` also holds the small shared primitives: `badge` (colored dot +
caption — meaning never rides on color alone), `pill` (small bordered tinted
chip for compact facts: OS name, kernel, arch, roles), `metric_tile`, `card`,
`section_header`, `empty_state`.

Prefer building shared widgets here (and reusing them from views) over
hand-rolling styled widgets in a view module — that keeps both the look and the
color-guard compliance in one place.

## Adding a color: checklist

1. Is it theme-dependent? Add a `ThemeColors` accessor in `theme.rs`.
2. Is it a fixed semantic color (status/severity/level/accent)? Add a
   `pub const` to the right palette group in `theme.rs`.
3. Is it a data-series color inside a shared widget? Build it with `kit::rgb` /
   `kit::rgba` under `view/components/`.
4. Never inline `Color::from_rgb(...)` in a `view/*.rs` module — the CI guard
   will reject it.
