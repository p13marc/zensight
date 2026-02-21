# Plan 05: Bridge Robustness

**Priority:** Medium
**Estimated effort:** 3-4 days
**Risk:** Medium (protocol-specific changes require testing with real devices)
**Crates affected:** `zenoh-bridge-snmp`, `zenoh-bridge-syslog`, `zenoh-bridge-netflow`, `zenoh-bridge-modbus`, `zenoh-bridge-gnmi`

---

## Objective

Fix concurrency, overflow, and error handling issues across all protocol bridge implementations.

---

## Task 1: gNMI Exponential Backoff for Reconnection

**Ref:** Analysis 2.7
**File:** `zenoh-bridge-gnmi/src/subscriber.rs:43-56`

### Problem

Reconnects every 5 seconds forever. No backoff, no attempt counter.

### Implementation

```rust
let mut backoff = Duration::from_secs(5);
let max_backoff = Duration::from_secs(300); // 5 minutes
let mut attempt = 0u64;

loop {
    attempt += 1;
    tracing::info!(attempt, backoff_secs = backoff.as_secs(), "Connecting to gNMI target");

    match self.connect_and_subscribe().await {
        Ok(()) => {
            // Reset on success
            backoff = Duration::from_secs(5);
            attempt = 0;
        }
        Err(e) => {
            tracing::warn!(
                attempt,
                error = %e,
                next_retry_secs = backoff.as_secs(),
                "gNMI connection failed"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}
```

---

## Task 2: Fix gNMI Nanosecond Overflow

**Ref:** Analysis 3.9
**File:** `zenoh-bridge-gnmi/src/subscriber.rs:176, 178`

### Problem

`sample_interval_ms * 1_000_000` overflows for large intervals.

### Implementation

Use `checked_mul` or `saturating_mul`:

```rust
// BEFORE:
sample_interval: sub.sample_interval_ms * 1_000_000,

// AFTER:
sample_interval: sub.sample_interval_ms.saturating_mul(1_000_000),
```

Apply to all nanosecond conversions (lines 176, 178, 223).

---

## Task 3: Fix SNMP Mutex Held Across Await

**Ref:** Analysis 3.5
**File:** `zenoh-bridge-snmp/src/poller.rs:187-190, 224-229`

### Problem

Mutex lock held during `timeout()` and network I/O.

### Implementation

Restructure to release lock before await:

```rust
// BEFORE:
let session = self.v3_session.lock().await;
let result = timeout(self.timeout, session.get(oid)).await;

// AFTER:
let result = {
    let session = self.v3_session.lock().await;
    let fut = session.get(oid);
    drop(session); // Release lock before waiting
    timeout(self.timeout, fut).await
};
```

Note: This only works if `session.get()` returns a future that doesn't borrow the session. If it does borrow, need to restructure more deeply (e.g., clone the session handle or use a channel).

### Investigation Required

Read the SNMP library docs to determine if the get/walk futures borrow the session. If they do, consider:
- Using a dedicated task per session with a channel-based API
- Cloning the session for each operation

---

## Task 4: Fix Modbus Address Overflow

**Ref:** Analysis 3.6
**File:** `zenoh-bridge-modbus/src/poller.rs:122`

### Problem

`register.address + addr_offset as u16` can overflow.

### Implementation

```rust
// BEFORE:
let address = register.address + addr_offset as u16;

// AFTER:
let address = register.address.checked_add(addr_offset as u16)
    .ok_or_else(|| anyhow::anyhow!(
        "Register address overflow: {} + {} exceeds u16 range",
        register.address, addr_offset
    ))?;
```

---

## Task 5: Fix Syslog `glob_to_regex()` Escaping

**Ref:** Analysis 3.8
**File:** `zenoh-bridge-syslog/src/filter.rs:123-141`

### Problem

Incomplete regex metacharacter escaping in glob-to-regex conversion.

### Implementation

Use the `regex::escape()` function for non-glob characters instead of manual escaping:

```rust
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::with_capacity(glob.len() * 2);
    regex.push('^');
    for ch in glob.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            // Escape everything else that's special in regex
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            | '^' | '$' | '\\' | '<' | '>' | '!' | '#' | '&' => {
                regex.push('\\');
                regex.push(ch);
            }
            _ => regex.push(ch),
        }
    }
    regex.push('$');
    regex
}
```

### Tests

```rust
#[test]
fn test_glob_special_chars() {
    let regex = glob_to_regex("app.name+version");
    assert!(Regex::new(&regex).unwrap().is_match("app.name+version"));
    assert!(!Regex::new(&regex).unwrap().is_match("appXnameXversion"));
}
```

---

## Task 6: Fix NetFlow Mutex Bottleneck

**Ref:** Analysis 4.5
**File:** `zenoh-bridge-netflow/src/receiver.rs:103`

### Problem

Single mutex serializes all packet processing per exporter.

### Implementation

Options:
1. **Shard the parser by source IP**: Use a `HashMap<IpAddr, Mutex<Parser>>` instead of one global mutex
2. **Make the parser lock-free**: If the parser is stateful only for template caching, use a `DashMap` for templates
3. **Clone parser per packet**: If parser state is cheap to clone

Recommended approach (sharded by exporter):

```rust
// Instead of:
let parser: Arc<Mutex<Parser>> = ...;

// Use:
let parsers: Arc<DashMap<IpAddr, Parser>> = Arc::new(DashMap::new());

// In process_packet:
let mut parser = parsers.entry(exporter_ip).or_insert_with(Parser::new);
let flows = parser.parse(data);
```

---

## Validation

```bash
cargo test -p zenoh-bridge-snmp
cargo test -p zenoh-bridge-syslog
cargo test -p zenoh-bridge-netflow
cargo test -p zenoh-bridge-modbus
cargo test -p zenoh-bridge-gnmi
cargo clippy --workspace -- --deny warnings
```

## Success Criteria

- [ ] gNMI reconnection uses exponential backoff (5s -> 10s -> 20s -> ... -> 300s max)
- [ ] No integer overflow in nanosecond conversions
- [ ] SNMP mutex not held across network I/O
- [ ] Modbus address arithmetic uses checked_add
- [ ] Syslog glob-to-regex escapes all metacharacters
- [ ] NetFlow packet processing is not serialized through a single mutex
- [ ] All bridge tests pass
