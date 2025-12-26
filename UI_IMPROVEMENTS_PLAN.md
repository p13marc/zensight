# UI Improvements Plan: iced_aw and iced_anim Integration

## Overview

This plan outlines the integration of two Iced ecosystem libraries to improve ZenSight's UI:
- **iced_aw 0.13** - Additional widgets (tabs, cards, badges, menus)
- **iced_anim 0.3** - Smooth animations and transitions

---

## Phase 1: Add iced_aw Widgets

### 1.1 Add Dependency

```toml
# zensight/Cargo.toml
[dependencies]
iced_aw = { version = "0.13", default-features = false, features = [
    "tabs",
    "card", 
    "badge",
    "number_input",
    "menu",
    "context_menu",
] }
```

### 1.2 Replace Protocol Tabs in Overview Section

**Current:** Custom button-based tab implementation in `view/overview/mod.rs`

**New:** Use `iced_aw::Tabs` widget

**Benefits:**
- Proper tab styling and active state
- Built-in keyboard navigation
- Consistent look and feel

**Files to modify:**
- `zensight/src/view/overview/mod.rs`

### 1.3 Add Badges to Dashboard

**Current:** Alert count shown as text "Alerts (3)"

**New:** Use `iced_aw::Badge` for:
- Unacknowledged alert count on Alerts button
- Device metric count badges
- Protocol count in overview tabs

**Files to modify:**
- `zensight/src/view/dashboard.rs`

### 1.4 Use Card Widget for Device List

**Current:** Custom container styling for device rows

**New:** Use `iced_aw::Card` for:
- Device cards on dashboard (header: device name, body: metrics summary)
- Alert cards in alerts view
- Rule cards in alerts view

**Benefits:**
- Consistent card styling with header/body/footer sections
- Built-in styling for different states

**Files to modify:**
- `zensight/src/view/dashboard.rs`
- `zensight/src/view/alerts.rs`

### 1.5 NumberInput for Numeric Settings

**Current:** Text input with manual parsing for thresholds

**New:** Use `iced_aw::NumberInput` for:
- Alert threshold input
- Stale threshold (seconds)
- Max history entries
- Max alerts count

**Benefits:**
- Built-in validation
- Increment/decrement buttons
- Proper number formatting

**Files to modify:**
- `zensight/src/view/alerts.rs`
- `zensight/src/view/settings.rs`

### 1.6 Context Menu for Devices and Topology Nodes

**Current:** Click to select, separate buttons for actions

**New:** Use `iced_aw::ContextMenu` for right-click menus:
- Device row: "View Details", "Add to Group", "Export"
- Topology node: "View Details", "Pin/Unpin", "Hide"
- Alert rule: "Edit", "Enable/Disable", "Delete"

**Benefits:**
- Discoverable actions
- Familiar desktop UX pattern

**Files to modify:**
- `zensight/src/view/dashboard.rs`
- `zensight/src/view/topology/mod.rs`
- `zensight/src/view/alerts.rs`

---

## Phase 2: Add iced_anim Animations

### 2.1 Add Dependency

```toml
# zensight/Cargo.toml
[dependencies]
iced_anim = { version = "0.3", features = ["widgets"] }
```

### 2.2 Animate View Transitions

**Current:** Instant view switches

**New:** Fade or slide transitions when switching between:
- Dashboard <-> Device Detail
- Dashboard <-> Alerts
- Dashboard <-> Topology
- Dashboard <-> Settings

**Implementation:**
- Wrap main content in `AnimationBuilder`
- Animate opacity or position

**Files to modify:**
- `zensight/src/app.rs` (view function)

### 2.3 Animate Theme Toggle

**Current:** Instant theme change

**New:** Smooth color transition when toggling dark/light mode

**Implementation:**
- Use `Animated<Theme>` or animate background colors
- Spring animation for smooth transition

**Files to modify:**
- `zensight/src/app.rs`

### 2.4 Animate Topology Layout

**Current:** Force-directed layout updates node positions directly

**New:** Animate node positions for smoother visual effect

**Implementation:**
- Use spring animations for node position updates
- Animate edge appearance when new connections form

**Files to modify:**
- `zensight/src/view/topology/graph.rs`
- `zensight/src/view/topology/layout.rs`

### 2.5 Button and Interactive Element Animations

**Current:** Static button appearance

**New:** Hover and press animations:
- Subtle scale on hover
- Color transitions on state change
- Ripple effect on click (if supported)

**Implementation:**
- Replace `iced::widget::button` with `iced_anim::widget::button`

**Files to modify:**
- All view files using buttons

### 2.6 Animate Alert Notifications

**Current:** Alerts appear instantly in list

**New:** 
- Slide-in animation for new alerts
- Fade-out when acknowledged
- Pulse animation for critical alerts

**Files to modify:**
- `zensight/src/view/alerts.rs`

---

## Phase 3: Polish and Refinement

### 3.1 Consistent Styling

- Define animation durations and curves as constants
- Create reusable animated components
- Ensure animations respect system accessibility settings (reduced motion)

### 3.2 Performance Testing

- Verify animations don't impact telemetry update performance
- Test with large numbers of devices (50+)
- Ensure topology animations remain smooth

### 3.3 Documentation

- Update CLAUDE.md with new widget usage patterns
- Document animation constants and customization

---

## Implementation Order

| Priority | Task | Effort | Impact |
|----------|------|--------|--------|
| 1 | Add iced_aw dependency | Low | Foundation |
| 2 | Replace overview tabs with Tabs widget | Medium | High |
| 3 | Add Badge to alert button | Low | Medium |
| 4 | Use Card for device list | Medium | High |
| 5 | NumberInput for settings | Medium | Medium |
| 6 | Add iced_anim dependency | Low | Foundation |
| 7 | Button hover animations | Low | Medium |
| 8 | Theme toggle animation | Low | Medium |
| 9 | View transition animations | Medium | High |
| 10 | Context menus | High | High |
| 11 | Topology animations | High | Medium |
| 12 | Alert animations | Medium | Medium |

---

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| iced_aw styling conflicts with current theme | Test incrementally, customize widget styles |
| Animation performance on low-end hardware | Add animation toggle in settings |
| Breaking changes in dependencies | Pin exact versions, test before updating |
| Increased binary size | Use feature flags to include only needed widgets |

---

## Success Criteria

- [ ] All protocol tabs use iced_aw Tabs widget
- [ ] Device cards use Card widget with consistent styling
- [ ] Alert count shows as Badge
- [ ] Numeric inputs use NumberInput with validation
- [ ] Right-click context menus work on devices and topology nodes
- [ ] View transitions are animated (can be disabled)
- [ ] Theme toggle animates smoothly
- [ ] No performance regression with animations enabled
- [ ] All existing tests pass

---

## Estimated Timeline

- **Phase 1 (iced_aw):** Core widget replacements
- **Phase 2 (iced_anim):** Animation layer
- **Phase 3 (Polish):** Refinement and testing

Each phase can be merged independently.
