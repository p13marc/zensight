# Iced 0.14 Frontend Improvements Plan

This document outlines opportunities to improve the ZenSight frontend by leveraging new features in Iced 0.14 and the `iced_anim` crate.

## Current State

ZenSight already uses:
- **Iced 0.14** with `tokio`, `canvas`, `svg` features
- **iced_anim 0.3** with `widgets` feature (button animations only)
- **iced_test 0.14** for headless UI testing

Current UI implementation relies heavily on:
- Manual list rendering with `Column`/`Row` for device lists and metrics
- Canvas-based custom widgets for charts, sparklines, and topology
- Custom components: Gauge, StatusLed, ProgressBar, Sparkline
- Manual pagination logic in dashboard

## Iced 0.14 New Features

| Feature | Description | Relevance to ZenSight |
|---------|-------------|----------------------|
| `table` widget | Structured data with sorting/filtering | High - device lists, syslog, metrics |
| `sensor` widget | Real-time data monitoring | High - telemetry display |
| `Animation` API | Native animation support | High - transitions, value updates |
| `grid` widget | Flexible grid layouts | Medium - dashboard layout |
| `float` widget | Advanced positioning | Low - tooltips, popovers |
| Double-click events | Mouse interaction | Medium - quick device open |
| Primitive culling | Performance optimization | Automatic benefit |
| Concurrent image decoding | Faster icon loading | Automatic benefit |
| Time travel debugging | Development tool | Development only |

## iced_anim Integration

The `iced_anim` crate (v0.3 for Iced 0.14) provides:

- **Spring animations** - Momentum-based, ideal for interactive elements
- **Transition animations** - Easing-curve-based for property changes
- **`Animated<T>` type** - Wraps values to track animation state
- **`Animation` widget** - Observes animated values and triggers rebuilds
- **Animatable types** - `f32`, `Color`, `Theme`, custom structs via derive

Current usage: Button hover animations only.

---

## Improvement Plan

### Phase 1: Quick Wins

#### 1.1 Expand iced_anim Usage

**Files:** `zensight/src/view/dashboard.rs`, `zensight/src/view/device.rs`

**Changes:**
- Add animated transitions when switching between views (dashboard → device detail)
- Animate status color changes (Online → Degraded → Offline)
- Animate metric value updates with number interpolation

**Implementation:**
```rust
use iced_anim::{Animated, Animation, Spring};

// In state
struct DeviceCardState {
    status_color: Animated<Color>,
}

// Update on status change
self.status_color.set_target(new_status.color());

// In view
Animation::new(&self.status_color, |color| {
    container(content).style(move |_| container::Style {
        background: Some(color.into()),
        ..Default::default()
    })
})
```

**Effort:** Low
**Impact:** Medium - Smoother, more polished UI feel

#### 1.2 Double-Click to Open Device

**Files:** `zensight/src/view/dashboard.rs`

**Changes:**
- Add double-click handler on device cards to open detail view
- Keep single-click for selection (if implementing multi-select later)

**Implementation:**
```rust
mouse_area(device_card)
    .on_double_click(Message::OpenDevice(device_id.clone()))
```

**Effort:** Low
**Impact:** Low - Convenience improvement

---

### Phase 2: Table Widget Adoption

#### 2.1 Syslog Message Table

**Files:** `zensight/src/view/specialized/syslog.rs`

**Current:** Manual row rendering with scrollable column
**Proposed:** Use `table` widget with sortable columns

**Columns:**
| Column | Type | Sortable |
|--------|------|----------|
| Severity | Badge | Yes |
| Timestamp | DateTime | Yes (default desc) |
| Facility | Text | Yes |
| App Name | Text | Yes |
| Message | Text (truncated) | No |

**Benefits:**
- Built-in column sorting
- Better performance for large log volumes
- Consistent table styling
- Column resize (if supported)

**Effort:** Medium
**Impact:** High - Major UX improvement for log analysis

#### 2.2 Device Metrics Table

**Files:** `zensight/src/view/device.rs`

**Current:** Manual scrollable list with custom row layout
**Proposed:** Table widget with columns

**Columns:**
| Column | Type | Sortable |
|--------|------|----------|
| Metric Name | Text | Yes |
| Value | Formatted | Yes (numeric) |
| Type | Badge | Yes |
| Trend | Icon | No |
| Updated | Relative time | Yes |
| Actions | Buttons | No |

**Benefits:**
- Sortable metrics (by name, value, recency)
- Consistent alignment
- Better keyboard navigation

**Effort:** Medium
**Impact:** Medium - Improved metric browsing

#### 2.3 Dashboard Device Table (Alternative View)

**Files:** `zensight/src/view/dashboard.rs`

**Current:** Card grid with pagination
**Proposed:** Add toggle between grid and table view

**Table Columns:**
| Column | Type | Sortable |
|--------|------|----------|
| Status | LED | Yes |
| Device | Text | Yes |
| Protocol | Icon+Text | Yes |
| Metrics | Count | Yes |
| Last Seen | Relative | Yes |

**Benefits:**
- Denser information display
- Quick sorting by any column
- Better for large device fleets

**Effort:** Medium
**Impact:** Medium - User preference option

---

### Phase 3: Sensor Widget Integration

#### 3.1 Evaluate Sensor Widget

**Task:** Review Iced 0.14 `sensor` widget API and determine applicability

**Status:** ✅ Evaluated - Not applicable for our use case

**Findings:**
The `sensor` widget is for detecting when content pops in and out of view (visibility detection), not for displaying sensor/telemetry data. It provides:
- `on_show` - triggered when content becomes visible
- `on_hide` - triggered when content goes out of view
- `on_resize` - triggered when visible content changes size
- `anticipate` - trigger early at a given distance before visibility

**Actual potential uses:**
- Lazy loading device cards as user scrolls (performance optimization)
- Triggering data fetching when elements come into view
- Infinite scroll implementations

**Not applicable for:**
- ~~Replace Gauge component for CPU/memory display~~
- ~~Real-time metric cards in overview~~
- ~~Network throughput indicators~~

**Conclusion:** Keep existing custom Gauge/Sparkline components. The sensor widget serves a different purpose (visibility detection vs data display).

**Effort:** N/A (not implementing)
**Impact:** N/A

---

### Phase 4: Animation Enhancements

#### 4.1 Chart Zoom/Pan Animations

**Files:** `zensight/src/view/chart.rs`

**Current:** Instant zoom/pan changes
**Proposed:** Smooth animated transitions

**Implementation:**
```rust
struct ChartState {
    zoom: Animated<f32>,
    pan_offset: Animated<f32>,
}

// On zoom button click
self.zoom.set_target(new_zoom_level);

// In draw(), use interpolated values
let current_zoom = self.zoom.value();
```

**Effort:** Medium
**Impact:** Medium - More intuitive chart interaction

#### 4.2 Topology Node Animations

**Files:** `zensight/src/view/topology/mod.rs`, `layout.rs`

**Current:** Force-directed layout with instant position updates
**Proposed:** Spring-based node movement for smoother settling

**Implementation:**
```rust
struct TopologyNode {
    position: Animated<Point>,
    // ...
}

// During layout step
node.position.set_target(calculated_position);
```

**Benefits:**
- Smoother graph settling
- Visual feedback during layout
- More organic feel

**Effort:** Medium
**Impact:** Medium - Better topology visualization

#### 4.3 Page Transition Animations

**Files:** `zensight/src/app.rs`

**Current:** Instant view switches
**Proposed:** Fade or slide transitions between major views

**Views to animate:**
- Dashboard ↔ Device Detail
- Dashboard ↔ Settings
- Dashboard ↔ Alerts
- Dashboard ↔ Topology

**Status:** ⚠️ Partial Implementation

**What was implemented:**
- Added `view_transition_key: u32` field to `ZenSight` struct
- Added `set_view()` helper method that increments the transition key on view changes
- Updated all view change locations to use `set_view()` instead of direct assignment
- Infrastructure is in place for future animation support

**Limitation encountered:**
The `iced_anim::AnimationBuilder` requires a `Fn` closure (callable multiple times), but view elements can only be consumed once (`FnOnce`). This makes wrapping the entire view in an animation builder impractical without restructuring the view function to reconstruct the view on each animation frame.

**Future options:**
1. Wait for iced to add native view transition support
2. Animate individual properties (opacity, transform) at the container level using widget-level animations
3. Use a state machine approach with separate "transitioning" view states

**Effort:** Medium
**Impact:** Low-Medium - Polish

---

### Phase 5: Grid Widget for Dashboard

#### 5.1 Replace Manual Grid Layout

**Files:** `zensight/src/view/dashboard.rs`

**Current:** Manual calculation of cards per row based on container width
**Proposed:** Use `grid` widget for responsive layout

**Benefits:**
- Automatic responsive behavior
- Cleaner code
- Consistent spacing

**Effort:** Low-Medium
**Impact:** Low - Code simplification

---

### Phase 6: Testing Enhancements

#### 6.1 Time Travel Debugging

**Task:** Integrate Iced 0.14's time travel debugging for development

**Benefits:**
- Step through UI state changes
- Debug complex interactions
- Record/replay bug scenarios

**Effort:** Low
**Impact:** Development productivity

#### 6.2 Enhanced Headless Testing

**Files:** `zensight/tests/ui_tests.rs`

**Current:** Basic simulator tests
**Proposed:** Leverage Iced 0.14's improved testing features

**Additions:**
- Animation state assertions
- Table interaction tests
- More comprehensive coverage

**Effort:** Medium
**Impact:** Test reliability

---

## Implementation Priority

| Priority | Item | Effort | Impact | Status |
|----------|------|--------|--------|--------|
| 1 | 1.1 Expand iced_anim (status colors, transitions) | Low | Medium | ✅ Done |
| 2 | 2.1 Syslog table widget | Medium | High | ✅ Done |
| 3 | 4.2 Topology node animations | Medium | Medium | ✅ Already implemented (force-directed layout) |
| 4 | 2.2 Device metrics table | Medium | Medium | ✅ Done (uses table widget with trend indicators) |
| 5 | 4.1 Chart zoom/pan animations | Medium | Medium | ✅ Already has feedback overlays |
| 6 | 1.2 Double-click to open | Low | Low | ✅ Done |
| 7 | 2.3 Dashboard table view | Medium | Medium | ✅ Done |
| 8 | 5.1 Grid widget for dashboard | Low | Low | ✅ Done |
| 9 | 4.3 Page transitions | Medium | Low | ⚠️ Partial (infrastructure added, animation deferred - AnimationBuilder requires Fn closures) |
| 10 | 3.1 Sensor widget evaluation | Low | TBD | ✅ Evaluated (N/A) |

---

## Dependencies

```toml
[dependencies]
iced = { version = "0.14", features = ["tokio", "canvas", "svg"] }
iced_anim = { version = "0.3", features = ["derive", "widgets"] }

[dev-dependencies]
iced_test = "0.14"
```

Note: The `derive` feature in `iced_anim` enables `#[derive(Animate)]` for custom structs.

---

## Breaking Changes to Consider

From Iced 0.14 release notes:
- `Widget::update` now accepts `Event` by reference
- `Task::perform` bound relaxed from `Fn` to `FnOnce`
- Removed `is_over` method from `Overlay` trait
- Removed color macro shorthand notation

These should already be handled since ZenSight targets Iced 0.14.

---

## Next Steps

1. Review this plan and prioritize based on user needs
2. Create GitHub issues for approved items
3. Implement Phase 1 items as proof of concept
4. Evaluate `table` and `sensor` widget APIs in detail before Phase 2/3
