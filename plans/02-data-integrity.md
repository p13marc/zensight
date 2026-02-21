# Plan 02: Data Integrity & Serialization

**Priority:** High
**Estimated effort:** 2-3 days
**Risk:** Medium (touches shared data model used everywhere)
**Crates affected:** `zensight-common`, all consumers

---

## Objective

Fix data integrity issues in the common data model that affect type precision, key expression handling, and status validation across all components.

---

## Task 1: Fix `i64` to `f64` Precision Loss

**Ref:** Analysis 2.1
**File:** `zensight-common/src/telemetry.rs:85-88`

### Problem

`From<i64>` for `TelemetryValue` silently casts to `f64`, losing precision for values > 2^53.

### Implementation

1. **Remove the implicit `From<i64>`** conversion or make it explicit:

```rust
// Option A: Remove From<i64> entirely, force callers to choose:
// (Remove the impl From<i64> for TelemetryValue block)

// Option B: Keep it but use Counter for non-negative, document the tradeoff:
impl From<i64> for TelemetryValue {
    fn from(v: i64) -> Self {
        if v >= 0 {
            TelemetryValue::Counter(v as u64)
        } else {
            TelemetryValue::Gauge(v as f64)
        }
    }
}
```

2. **Search all callsites** that use `.into()` or `From<i64>`:

```bash
grep -rn "From<i64>\|\.into()\|TelemetryValue::from" --include="*.rs"
```

3. Update each callsite to use the explicit constructor (`Counter(v as u64)` or `Gauge(v as f64)`) based on intent.

### Tests

```rust
#[test]
fn test_large_i64_preserved() {
    let large: i64 = i64::MAX;
    // Ensure no silent precision loss
    let val = TelemetryValue::Counter(large as u64);
    if let TelemetryValue::Counter(v) = val {
        assert_eq!(v, large as u64);
    }
}
```

---

## Task 2: Improve `parse_key_expr()` Error Reporting

**Ref:** Analysis 3.2
**File:** `zensight-common/src/keyexpr.rs:185`

### Problem

Returns `Option<ParsedKeyExpr>` with no context on failure.

### Implementation

1. Define a `ParseError` enum:

```rust
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("key expression too short: expected at least 4 segments, got {0}")]
    TooFewSegments(usize),
    #[error("invalid prefix: expected '{expected}', got '{actual}'")]
    InvalidPrefix { expected: &'static str, actual: String },
    #[error("unknown protocol: '{0}'")]
    UnknownProtocol(String),
    #[error("empty source identifier")]
    EmptySource,
}
```

2. Change `parse_key_expr` signature:

```rust
pub fn parse_key_expr(key: &str) -> Result<ParsedKeyExpr<'_>, ParseError> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() < 4 {
        return Err(ParseError::TooFewSegments(parts.len()));
    }
    if parts[0] != KEY_PREFIX {
        return Err(ParseError::InvalidPrefix {
            expected: KEY_PREFIX,
            actual: parts[0].to_string(),
        });
    }
    let protocol = match parts[1] {
        "snmp" => Protocol::Snmp,
        // ... all protocols including sysinfo ...
        other => return Err(ParseError::UnknownProtocol(other.to_string())),
    };
    // ...
}
```

3. **Update all callsites** that call `parse_key_expr()`:
   - Frontend subscription: likely `.ok()` to maintain current behavior
   - Exporters: log the error before discarding

---

## Task 3: Add Key Expression Input Validation

**Ref:** Analysis 3.1
**Files:** `zensight-common/src/keyexpr.rs:44-51`, `zensight-bridge-framework/src/publisher.rs:50-56`

### Problem

`KeyExprBuilder::build()` and `Publisher::build_key()` accept empty or malformed inputs.

### Implementation

1. Add validation to `KeyExprBuilder::build()`:

```rust
pub fn build(&self, source: &str, metric: &str) -> Result<String, ParseError> {
    if source.is_empty() {
        return Err(ParseError::EmptySource);
    }
    if source.contains("//") || metric.contains("//") {
        return Err(ParseError::InvalidCharacters);
    }
    Ok(format!("{}/{}/{}/{}", self.prefix, self.protocol.as_str(), source, metric))
}
```

2. Add similar validation to `Publisher::build_key()`.

---

## Task 4: Replace String-Typed Status Fields with Enum

**Ref:** Analysis 3.3
**File:** `zensight-common/src/health.rs:40-41, 142`

### Problem

`HealthSnapshot.status` and `BridgeInfo.status` are `String`.

### Implementation

1. Define a `BridgeStatus` enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeStatus {
    Healthy,
    Degraded,
    Unhealthy,
    Starting,
    Stopping,
}

impl std::fmt::Display for BridgeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Unhealthy => write!(f, "unhealthy"),
            Self::Starting => write!(f, "starting"),
            Self::Stopping => write!(f, "stopping"),
        }
    }
}
```

2. Replace `status: String` with `status: BridgeStatus` in `HealthSnapshot` and `BridgeInfo`.
3. Update all sites that construct these structs.
4. Update frontend display logic (likely `format!("{}", status)` calls).

---

## Validation

```bash
cargo test --workspace
cargo clippy --workspace -- --deny warnings
```

## Success Criteria

- [ ] No implicit `i64` to `f64` conversion in `TelemetryValue`
- [ ] `parse_key_expr()` returns `Result` with descriptive errors
- [ ] Key expression builder rejects empty source/metric
- [ ] All status fields use `BridgeStatus` enum
- [ ] All workspace tests pass
