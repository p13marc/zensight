# Plan 03: Bridge Framework Hardening

**Priority:** High
**Estimated effort:** 3-4 days
**Risk:** Medium (framework changes affect all bridges)
**Crates affected:** `zensight-bridge-framework`, all `zenoh-bridge-*`

---

## Objective

Harden the bridge framework to prevent silent failures, handle concurrency correctly, and provide proper lifecycle management.

---

## Task 1: Implement Rolling Window for Error Counter

**Ref:** Analysis 2.3
**File:** `zensight-bridge-framework/src/health.rs:38, 283`

### Problem

`errors_last_hour` only increments, never resets. Name is misleading.

### Implementation

Replace the atomic counter with a time-bucketed ring buffer:

```rust
use std::sync::Mutex;

/// Rolling window error counter with 1-minute buckets.
struct RollingErrorCounter {
    /// 60 buckets, one per minute for the last hour.
    buckets: Mutex<[u64; 60]>,
    /// Current bucket index.
    current_bucket: AtomicUsize,
    /// Timestamp of last bucket rotation.
    last_rotation: AtomicI64,
}

impl RollingErrorCounter {
    fn increment(&self) {
        self.rotate_if_needed();
        let idx = self.current_bucket.load(Ordering::SeqCst);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets[idx] += 1;
    }

    fn count(&self) -> u64 {
        self.rotate_if_needed();
        let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.iter().sum()
    }

    fn rotate_if_needed(&self) {
        // Check if a minute has passed, zero out expired buckets
    }
}
```

### Also

Update `HealthSnapshot.errors_last_hour` to use the rolling count.

---

## Task 2: Handle Lock Poisoning Gracefully

**Ref:** Analysis 3.4
**File:** `zensight-bridge-framework/src/correlation.rs:143-155`

### Problem

`.unwrap()` on `RwLock` operations causes cascading panics.

### Implementation

Replace all `.unwrap()` on lock acquisitions with `.unwrap_or_else(|e| e.into_inner())`:

```rust
// BEFORE:
let mut entries = self.entries.write().unwrap();

// AFTER:
let mut entries = self.entries.write().unwrap_or_else(|poisoned| {
    tracing::warn!("Lock was poisoned, recovering");
    poisoned.into_inner()
});
```

Apply to all occurrences in `correlation.rs` (lines 143, 152, 184, 192, 201, 207, 213, 224, 243).

---

## Task 3: Add Graceful Shutdown with Cancellation Tokens

**Ref:** Analysis 3.7
**File:** `zensight-bridge-framework/src/runner.rs:213-238`

### Problem

Worker tasks are aborted without cleanup.

### Implementation

1. Add `tokio_util` dependency for `CancellationToken`:

```toml
# In Cargo.toml:
tokio-util = { version = "0.7", features = ["rt"] }
```

2. Create a cancellation token in `BridgeRunner`:

```rust
pub struct BridgeRunner {
    // ...existing fields...
    cancel_token: CancellationToken,
}
```

3. Expose the token for workers:

```rust
impl BridgeRunner {
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.child_token()
    }
}
```

4. Replace `task.abort()` with token cancellation + timeout:

```rust
async fn shutdown(&self) {
    self.cancel_token.cancel();

    // Give workers 5 seconds to clean up
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::select! {
        _ = futures::future::join_all(self.tasks.iter().map(|t| t)) => {
            tracing::info!("All workers shut down gracefully");
        }
        _ = deadline => {
            tracing::warn!("Shutdown timeout, aborting remaining workers");
            for task in &self.tasks {
                task.abort();
            }
        }
    }
}
```

5. Update bridge implementations to check the token:

```rust
// In bridge worker loops:
loop {
    tokio::select! {
        _ = cancel_token.cancelled() => break,
        result = poll_device() => { /* handle */ }
    }
}
```

---

## Task 4: Add Backpressure to Publisher

**Ref:** Analysis improvement suggestion (section 6.1)
**File:** `zensight-bridge-framework/src/publisher.rs:93-113`

### Problem

`publish_batch()` has no flow control. If Zenoh is slow, publishes pile up.

### Implementation

Add optional rate limiting configuration:

```rust
pub struct PublisherConfig {
    /// Maximum publishes per second (0 = unlimited).
    pub max_rate: u32,
    /// Maximum outstanding publishes before backpressure.
    pub max_outstanding: usize,
}
```

For the initial version, add a simple semaphore-based approach:

```rust
pub async fn publish(&self, key_suffix: &str, point: &TelemetryPoint) -> Result<()> {
    let _permit = self.semaphore.acquire().await?;
    // ... existing publish logic ...
}
```

---

## Task 5: Improve Error Categorization

**Ref:** Analysis framework issue (BridgeError)
**File:** `zensight-bridge-framework/src/error.rs:97-101`

### Problem

All Zenoh errors map to a single `ZenohSession` variant.

### Implementation

```rust
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("Zenoh connection error: {0}")]
    ZenohConnection(String),
    #[error("Zenoh publish error: {0}")]
    ZenohPublish(String),
    #[error("Zenoh subscription error: {0}")]
    ZenohSubscription(String),
    // ... keep existing variants ...
}
```

---

## Validation

```bash
cargo test -p zensight-bridge-framework
cargo test --workspace  # Ensure bridges still compile and pass
cargo clippy --workspace -- --deny warnings
```

## Success Criteria

- [ ] `errors_last_hour` reports actual rolling window count
- [ ] No `.unwrap()` on lock acquisitions in production code
- [ ] Workers receive cancellation tokens and shut down gracefully within 5s
- [ ] Publisher supports configurable backpressure
- [ ] Zenoh errors are categorized (connection vs publish vs subscription)
- [ ] All workspace tests pass
