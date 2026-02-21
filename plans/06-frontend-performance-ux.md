# Plan 06: Frontend Performance & UX

**Priority:** Medium
**Estimated effort:** 3-5 days
**Risk:** Low (frontend-only changes)
**Crates affected:** `zensight`

---

## Objective

Improve frontend performance for large device counts and add essential UX features for user feedback and notification.

---

## Part A: Performance

### Task 1: Replace `Vec` with `VecDeque` for Metric History

**Ref:** Analysis 4.1
**File:** `zensight/src/view/device.rs:103, 121`

### Problem

`Vec::remove(0)` is O(n). Called on every telemetry update.

### Implementation

1. Change the history storage type:

```rust
// BEFORE:
pub history: HashMap<String, Vec<DataPoint>>,

// AFTER:
use std::collections::VecDeque;
pub history: HashMap<String, VecDeque<DataPoint>>,
```

2. Replace `remove(0)` with `pop_front()`:

```rust
// BEFORE:
while history.len() > max_history {
    history.remove(0);
}

// AFTER:
while history.len() > self.max_history {
    history.pop_front();
}
```

3. Update chart rendering to work with `VecDeque` (it implements `Index` and iterators, so most code should work unchanged).

4. If chart code uses slice indexing (`&data[start..end]`), use `.make_contiguous()` or convert with `.iter().collect::<Vec<_>>()` at render time.

---

### Task 2: Reduce Cloning in Telemetry Hot Path

**Ref:** Analysis 4.2
**File:** `zensight/src/app.rs` (multiple locations)

### Problem

`DeviceId`, `TelemetryPoint`, `ZenohConfig` cloned on every metric update.

### Implementation

Audit and reduce cloning:

1. **DeviceId**: Consider using `Arc<DeviceId>` or storing devices by index
2. **TelemetryPoint**: Pass by reference where possible. Only clone when storing
3. **ZenohConfig**: Clone once at subscription start, not on every reconnection attempt

Priority: Focus on the `Message::TelemetryReceived` handler, which is the hottest path.

---

### Task 3: Cache Dashboard Filtered Results

**Ref:** Analysis 4.3
**File:** `zensight/src/view/dashboard.rs:219-228`

### Problem

`filtered_devices()` re-runs on every tick.

### Implementation

Add a cache invalidation pattern:

```rust
pub struct DashboardState {
    // ... existing fields ...
    /// Cached filtered device list (invalidated on filter/device change).
    filtered_cache: Option<Vec<DeviceId>>,
    /// Filter version counter for cache invalidation.
    filter_version: u64,
}

impl DashboardState {
    pub fn set_search_filter(&mut self, filter: String) {
        self.search_filter = filter;
        self.filtered_cache = None; // Invalidate
    }

    pub fn filtered_devices(&mut self) -> &[DeviceId] {
        if self.filtered_cache.is_none() {
            self.filtered_cache = Some(self.compute_filtered_devices());
        }
        self.filtered_cache.as_ref().unwrap()
    }

    fn invalidate_filter_cache(&mut self) {
        self.filtered_cache = None;
    }
}
```

Invalidate the cache when:
- A new device is added
- A device is removed
- The search filter changes
- The sort order changes

---

### Task 4: Reduce String Allocations in Subscription

**Ref:** Analysis 4.4
**File:** `zensight/src/subscription.rs:166-184`

### Problem

`parse_bridge_liveliness()` allocates strings on every sample.

### Implementation

Use `Cow<str>` or parse without allocating where possible:

```rust
// If the function currently does:
fn parse_bridge_liveliness(key: &str) -> Option<(String, String)> {
    // ... splits and allocates ...
}

// Change to return references:
fn parse_bridge_liveliness(key: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = key.split('/').collect();
    // ... return borrowed slices ...
}
```

---

## Part B: UX Improvements

### Task 5: Add Loading Indicator During Zenoh Connection

**Ref:** Analysis 5.1
**File:** `zensight/src/app.rs`

### Problem

No visual feedback during 5-second connection timeout.

### Implementation

1. Add a `ConnectionState` enum:

```rust
enum ConnectionState {
    Disconnected,
    Connecting { since: Instant },
    Connected,
    Reconnecting { attempt: u32, since: Instant },
}
```

2. Show a loading spinner or "Connecting..." banner based on state.
3. Update state transitions in the subscription handler.

---

### Task 6: Add Toast Notification System

**Ref:** Analysis 5.6
**File:** `zensight/src/app.rs`, new file `zensight/src/view/toast.rs`

### Problem

Errors only visible in terminal logs.

### Implementation

1. Create a `ToastNotification` model:

```rust
pub struct ToastNotification {
    pub id: u64,
    pub message: String,
    pub severity: ToastSeverity,
    pub created_at: Instant,
    pub duration: Duration,
}

pub enum ToastSeverity {
    Info,
    Warning,
    Error,
    Success,
}
```

2. Add a `Vec<ToastNotification>` to the app state.
3. Render as overlay in the bottom-right corner.
4. Auto-dismiss after `duration` (default 5s for info, 10s for error).
5. Use for: export errors, settings save results, connection state changes, alert triggers.

---

### Task 7: Surface Export Errors to User

**Ref:** Analysis 5.4
**File:** `zensight/src/app.rs:1100-1105`

### Problem

CSV/JSON export failures only logged.

### Implementation

After implementing the toast system (Task 6):

```rust
// BEFORE:
if let Err(e) = std::fs::write(&path, csv) {
    tracing::error!(error = %e, "Failed to export CSV");
}

// AFTER:
match std::fs::write(&path, csv) {
    Ok(()) => self.show_toast(ToastSeverity::Success, format!("Exported to {}", path)),
    Err(e) => self.show_toast(ToastSeverity::Error, format!("Export failed: {}", e)),
}
```

---

### Task 8: Add Stale Metric Visual Indicator

**Ref:** Analysis 5.3
**File:** `zensight/src/view/chart.rs`, `zensight/src/view/device.rs`

### Problem

No visual distinction between fresh and stale metrics.

### Implementation

1. Define staleness threshold (configurable, default 60s):

```rust
const STALE_THRESHOLD_SECS: i64 = 60;
```

2. In the device metric list, show stale metrics with reduced opacity or a gray badge:

```rust
let is_stale = current_timestamp() - point.timestamp > STALE_THRESHOLD_SECS * 1000;
let text_color = if is_stale { Color::from_rgb(0.5, 0.5, 0.5) } else { text_color };
```

3. In charts, draw a vertical dashed line at the "last update" timestamp.

---

## Validation

```bash
cargo test -p zensight
cargo clippy -p zensight -- --deny warnings
# Manual testing: run with --demo and verify UI behavior
```

## Success Criteria

- [ ] Metric history uses `VecDeque` with O(1) `pop_front()`
- [ ] Dashboard filtering is cached and invalidated correctly
- [ ] Subscription parsing avoids unnecessary string allocations
- [ ] Loading indicator shown during Zenoh connection
- [ ] Toast notifications appear for errors and confirmations
- [ ] Export success/failure shown to user via toast
- [ ] Stale metrics visually distinguishable from fresh ones
- [ ] All frontend tests pass
