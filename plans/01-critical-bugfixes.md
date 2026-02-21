# Plan 01: Critical Bug Fixes

**Priority:** Immediate
**Estimated effort:** 1-2 days
**Risk:** Low (small, targeted changes)
**Crates affected:** `zensight-common`, `zensight-bridge-framework`

---

## Objective

Fix the three critical bugs identified in the analysis. These are correctness issues that cause silent data loss or violate memory safety.

---

## Task 1: Add Missing `Sysinfo` Protocol to `parse_key_expr()`

**File:** `zensight-common/src/keyexpr.rs:192-199`
**Severity:** Critical
**Effort:** 5 minutes

### Problem

The `parse_key_expr()` function is missing `"sysinfo"` in the protocol match arm. All sysinfo key expressions silently fail to parse, returning `None`.

### Implementation

1. Open `zensight-common/src/keyexpr.rs`
2. Add `"sysinfo" => Protocol::Sysinfo,` to the match block at line 198 (before `_ => return None`)
3. Add a test case for sysinfo parsing

### Code Change

```rust
// In parse_key_expr(), add to the match block:
let protocol = match parts[1] {
    "snmp" => Protocol::Snmp,
    "syslog" => Protocol::Syslog,
    "gnmi" => Protocol::Gnmi,
    "netflow" => Protocol::Netflow,
    "opcua" => Protocol::Opcua,
    "modbus" => Protocol::Modbus,
    "sysinfo" => Protocol::Sysinfo,  // <-- ADD THIS
    _ => return None,
};
```

### Test to Add

```rust
#[test]
fn test_parse_sysinfo_key_expr() {
    let parsed = parse_key_expr("zensight/sysinfo/server01/cpu/usage").unwrap();
    assert_eq!(parsed.protocol, Protocol::Sysinfo);
    assert_eq!(parsed.source, "server01");
    assert_eq!(parsed.metric, "cpu/usage");
}
```

### Validation

```bash
cargo test -p zensight-common test_parse
```

---

## Task 2: Remove Unsafe `transmute` in AdvancedPublisherRegistry

**File:** `zensight-bridge-framework/src/advanced_publisher.rs:183`
**Severity:** Critical
**Effort:** 2-4 hours

### Problem

`unsafe { std::mem::transmute(publisher) }` converts a borrowing lifetime to `'static`. This violates Rust's memory safety guarantees and can cause use-after-free if the session is dropped before the publisher.

### Investigation Steps

1. Read `advanced_publisher.rs` fully to understand the lifetime relationship between `Session`, `AdvancedPublisher`, and the registry
2. Check how `AdvancedPublisher` is declared in zenoh-ext (its lifetime parameter)
3. Determine if zenoh-ext provides an owned version or a way to construct with `Arc<Session>`

### Implementation Options

**Option A: Add Lifetime to Registry (preferred if feasible)**

```rust
pub struct AdvancedPublisherRegistry<'a> {
    session: Arc<Session>,
    publishers: RwLock<HashMap<String, AdvancedPublisher<'a>>>,
    // ...
}
```

This propagates the lifetime to all users but is the safest approach.

**Option B: Use `Session` directly for publishing**

If zenoh's `Session::put()` is sufficient (without needing the AdvancedPublisher's caching), replace the cached publisher pattern with direct session puts. This avoids the lifetime issue entirely.

**Option C: Restructure to ensure drop order**

If the `Arc<Session>` genuinely outlives all publishers, add a `Drop` implementation that explicitly clears publishers before releasing the session:

```rust
impl Drop for AdvancedPublisherRegistry {
    fn drop(&mut self) {
        // Clear all publishers before session could be dropped
        let mut publishers = self.publishers.blocking_write();
        publishers.clear();
    }
}
```

Combined with documentation of the safety invariant. This is the least disruptive but still uses unsafe.

### Also Fix: TOCTOU Race Condition (lines 156-191)

While modifying this file, also fix the double-checked locking race:

```rust
// BEFORE (racy):
let publishers = self.publishers.read().await;
if publishers.contains_key(key) { return Ok(()); }
drop(publishers);
// ... create publisher ...
let mut publishers = self.publishers.write().await;

// AFTER (safe):
let mut publishers = self.publishers.write().await;
if publishers.contains_key(key) { return Ok(()); }
// ... create publisher while holding write lock ...
publishers.insert(key.to_string(), publisher);
```

### Validation

```bash
cargo test -p zensight-bridge-framework
cargo clippy -p zensight-bridge-framework -- --deny warnings
```

---

## Task 3: Tag `TelemetryValue` Enum for Serialization

**File:** `zensight-common/src/telemetry.rs:60-77`
**Severity:** Critical (but breaking change)
**Effort:** 4-8 hours (includes updating all serialization/deserialization callsites)

### Problem

`#[serde(untagged)]` causes ambiguous deserialization. A JSON integer `42` always becomes `Counter(42)`, never `Gauge(42.0)`, even if the producer intended it as a gauge.

### Implementation

#### Step 1: Change the Enum Tagging

```rust
// BEFORE:
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TelemetryValue {
    Counter(u64),
    Gauge(f64),
    Text(String),
    Boolean(bool),
    Binary(Vec<u8>),
}

// AFTER:
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value")]
pub enum TelemetryValue {
    #[serde(rename = "counter")]
    Counter(u64),
    #[serde(rename = "gauge")]
    Gauge(f64),
    #[serde(rename = "text")]
    Text(String),
    #[serde(rename = "boolean")]
    Boolean(bool),
    #[serde(rename = "binary")]
    Binary(Vec<u8>),
}
```

#### Step 2: Verify JSON Format Change

Old format: `42` or `"hello"`
New format: `{"type": "counter", "value": 42}` or `{"type": "text", "value": "hello"}`

#### Step 3: Update All Affected Code

Search for all places that construct or parse `TelemetryValue` from raw JSON:

```bash
cargo build --workspace 2>&1  # Compiler errors will guide the changes
```

Key areas to check:
- `zensight-common/src/serialization.rs` -- roundtrip tests
- `zensight/src/subscription.rs` -- Zenoh sample deserialization
- All bridge crates -- TelemetryPoint construction (these create values programmatically, so they should be fine)
- Both exporter crates -- TelemetryPoint deserialization from Zenoh
- `zensight/src/mock.rs` -- mock data generation

#### Step 4: Update Tests

All serialization roundtrip tests need updating for the new format.

#### Step 5: Consider Migration

If there are existing deployments with old-format data:
- Add a temporary compatibility deserializer that tries tagged first, then falls back to untagged
- Remove the fallback in a future version

### Validation

```bash
cargo test --workspace
cargo clippy --workspace -- --deny warnings
```

---

## Rollout Plan

1. **Day 1 morning:** Task 1 (sysinfo fix) -- merge immediately
2. **Day 1 afternoon:** Task 2 (unsafe transmute) -- review options, implement
3. **Day 2:** Task 3 (tagged enum) -- implement with migration support, run full test suite

## Success Criteria

- [ ] `parse_key_expr("zensight/sysinfo/server01/cpu/usage")` returns `Some(...)` with `Protocol::Sysinfo`
- [ ] No `unsafe` blocks remain in `zensight-bridge-framework`
- [ ] `TelemetryValue` serialization/deserialization is unambiguous for all variant types
- [ ] All workspace tests pass: `cargo test --workspace`
- [ ] No new clippy warnings: `cargo clippy --workspace -- --deny warnings`
