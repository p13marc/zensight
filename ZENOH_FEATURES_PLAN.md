# Zenoh Advanced Features Integration Plan

## Overview

This plan outlines how to leverage Zenoh's advanced features (Query/Queryable, Liveliness, AdvancedPublisher/Subscriber) to improve ZenSight's reliability, performance, and user experience.

## Current State

ZenSight currently uses basic Zenoh primitives:
- **Bridges**: `session.put()` for publishing telemetry
- **Frontend**: `session.declare_subscriber()` for receiving telemetry
- **Custom health system**: Manual `@/health` and `@/devices/*/liveness` key expressions

This works but has limitations:
- Frontend must wait for fresh data on startup
- No way to query historical or on-demand data
- Bridge/device presence detection requires custom polling
- Network blips can cause data gaps

---

## Phase 1: Liveliness for Bridge & Device Presence

### Goal
Replace custom health/liveness system with native Zenoh liveliness tokens for instant, reliable presence detection.

### How Zenoh Liveliness Works
```rust
// Bridge declares liveliness token
let token = session.liveliness()
    .declare_token("zensight/snmp/@/alive")
    .await?;
// Token automatically disappears when bridge dies

// Frontend subscribes to liveliness
let subscriber = session.liveliness()
    .declare_subscriber("zensight/*/@/alive")
    .await?;
// Receives Put on appearance, Delete on disappearance
```

### Implementation

#### 1.1 Bridge-Level Liveliness

**Changes to bridges:**
```
Key expression: zensight/<protocol>/@/alive
```

Each bridge declares a liveliness token on startup. The token is automatically removed by Zenoh when the bridge process dies or loses network connectivity.

**Files to modify:**
- `zensight-bridge-framework/src/lib.rs` - Add liveliness token declaration
- Each bridge's main.rs - Ensure token is held for process lifetime

**Benefits:**
- Instant bridge online/offline detection (no polling delay)
- Works even if bridge crashes (Zenoh handles cleanup)
- Simpler than current HealthSnapshot publishing

#### 1.2 Device-Level Liveliness

**Changes to bridges:**
```
Key expression: zensight/<protocol>/@/devices/<device_id>/alive
```

Each bridge declares liveliness tokens for devices that are responding.

**Files to modify:**
- `zensight-bridge-framework/src/health.rs` - Declare/undeclare device tokens based on poll results

**Benefits:**
- Real-time device status in frontend
- No need for DeviceLiveness messages
- Automatic cleanup on bridge death

#### 1.3 Frontend Liveliness Subscriber

**Changes to frontend:**
```rust
// Subscribe to all bridge liveliness
session.liveliness().declare_subscriber("zensight/*/@/alive")

// Subscribe to all device liveliness  
session.liveliness().declare_subscriber("zensight/*/@/devices/*/alive")
```

**Files to modify:**
- `zensight/src/subscription.rs` - Add liveliness subscriptions
- `zensight/src/message.rs` - Add BridgeOnline/BridgeOffline, DeviceOnline/DeviceOffline messages
- `zensight/src/app.rs` - Handle presence messages

**Benefits:**
- Instant UI updates when bridges/devices appear/disappear
- Dashboard shows real-time bridge health without polling

### Migration Path
1. Implement liveliness alongside existing health system
2. Frontend uses liveliness as primary, falls back to health messages
3. Eventually deprecate custom health publishing

---

---

## Phase 2: Advanced Pub/Sub (Cache, History, Recovery)

### Goal
Use zenoh-ext advanced publisher/subscriber for:
- **Instant data on subscription** - Frontend gets cached samples immediately (replaces need for query)
- **Reliable delivery** - Automatic recovery of missed samples
- **Presence detection** - Know when publishers/subscribers appear/disappear

### How It Works

```rust
// Bridge: AdvancedPublisher with cache and miss detection
let publisher = session
    .declare_publisher(&key_expr)
    .cache(CacheConfig::default().max_samples(history))
    .sample_miss_detection(MissDetectionConfig::default().heartbeat(Duration::from_millis(500)))
    .publisher_detection()
    .await?;

// Frontend: AdvancedSubscriber with history and recovery
let subscriber = session
    .declare_subscriber(key_expr)
    .history(HistoryConfig::default().detect_late_publishers())
    .recovery(RecoveryConfig::default().heartbeat())
    .subscriber_detection()
    .await?;
```

### Key Features

| Feature | Side | Purpose |
|---------|------|---------|
| `cache()` | Publisher | Store last N samples for late-joining subscribers |
| `sample_miss_detection()` | Publisher | Detect when subscribers miss samples, enable recovery |
| `publisher_detection()` | Publisher | Allow subscribers to detect this publisher |
| `history()` | Subscriber | Fetch cached samples on subscription |
| `detect_late_publishers()` | Subscriber | Get history from publishers that appear after subscription |
| `recovery()` | Subscriber | Automatically recover missed samples |
| `subscriber_detection()` | Subscriber | Allow publishers to detect this subscriber |

### Implementation

#### 2.1 Bridges: AdvancedPublisher

**Changes:**
```rust
// Before (basic publisher)
session.put(&key, payload).await?;

// After (advanced publisher with cache)
let publisher = session
    .declare_publisher(&key_expr)
    .cache(CacheConfig::default().max_samples(100))
    .sample_miss_detection(MissDetectionConfig::default().heartbeat(Duration::from_millis(500)))
    .publisher_detection()
    .await?;

publisher.put(payload).await?;
```

**Files to modify:**
- `zensight-bridge-framework/Cargo.toml` - Add zenoh-ext dependency
- `zensight-bridge-framework/src/lib.rs` - Create publisher registry
- Each bridge's collector - Use declared publishers instead of session.put()

**Configuration:**
```json5
{
  publisher: {
    cache_size: 100,           // Samples to cache per key
    miss_detection: true,      // Enable sample miss detection
    heartbeat_ms: 500,         // Heartbeat interval for miss detection
  }
}
```

#### 2.2 Frontend: AdvancedSubscriber

**Changes:**
```rust
// Before (basic subscriber)
let subscriber = session.declare_subscriber(&key_expr).await?;

// After (advanced subscriber with history and recovery)
let subscriber = session
    .declare_subscriber(&key_expr)
    .history(HistoryConfig::default().detect_late_publishers())
    .recovery(RecoveryConfig::default().heartbeat())
    .subscriber_detection()
    .await?;
```

**Files to modify:**
- `zensight/Cargo.toml` - Add zenoh-ext dependency
- `zensight/src/subscription.rs` - Use AdvancedSubscriber

**Benefits:**
- **Instant data on startup**: Frontend immediately receives cached samples from all connected bridges
- **Instant data on device selection**: Charts populated with history, no "waiting for data"
- **Late publisher support**: If a bridge starts after the frontend, `detect_late_publishers()` fetches its cache
- **Automatic recovery**: Missed samples (network blips) are automatically retransmitted
- **No gaps**: Time-series data is complete even after reconnection

#### 2.3 Publisher/Subscriber Detection

The `publisher_detection()` and `subscriber_detection()` features provide mutual awareness:

- Frontend knows when a bridge's publisher appears/disappears
- Bridges know when the frontend subscribes

This can supplement or partially replace liveliness tokens for presence detection.

### What This Replaces

| Previous Plan | Replaced By |
|---------------|-------------|
| Phase 2: Query for current state | `history()` - cached samples delivered on subscription |
| Phase 3: History catch-up | `history()` + `recovery()` - automatic |
| Custom health polling | `publisher_detection()` - know when bridges appear |

### Interaction with Liveliness (Phase 1)

Advanced pub/sub detection complements liveliness:

| Feature | Use Case |
|---------|----------|
| Liveliness | Bridge process alive/dead (even if not publishing) |
| Publisher detection | Specific publisher key active/inactive |
| History | Get data from publishers that exist |

**Recommendation**: Use both:
- Liveliness for coarse-grained "is the bridge running?"
- Publisher detection for fine-grained "is this data stream active?"

---

## Phase 3: Historical Data Query (Future)

### Goal
Allow frontend to query historical data beyond the publisher cache using Zenoh's storage backend.

### Architecture: Zenoh Storage Backend

Deploy `zenoh-plugin-storage-manager` with a storage backend that automatically stores all telemetry published to `zensight/**`.

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Bridges   │────▶│   Zenoh     │────▶│  Frontend   │
│ (publishers)│     │   Router    │     │(subscribers)│
└─────────────┘     └──────┬──────┘     └──────┬──────┘
                           │                   │
                           ▼                   │
                    ┌─────────────┐            │
                    │   Storage   │◀───────────┘
                    │   Backend   │  (time-range queries)
                    └─────────────┘
```

### Storage Backend Options

| Backend | Use Case | Notes |
|---------|----------|-------|
| **Memory** | Development/testing | Fast, no persistence |
| **RocksDB** | General purpose | Good performance, local storage |
| **InfluxDB** | Time-series focus | Native time-range queries, downsampling |
| **S3** | Cloud/archival | Scalable, cost-effective for cold data |

**Recommendation**: Start with RocksDB for simplicity, migrate to InfluxDB if advanced time-series features needed.

### Router Configuration

```json5
{
  plugins: {
    storage_manager: {
      storages: {
        zensight_telemetry: {
          key_expr: "zensight/**",
          volume: {
            id: "rocksdb",
            path: "/var/lib/zensight/storage",
          },
          // Optional: retention policy
          strip_prefix: "zensight",
        }
      },
      volumes: {
        rocksdb: {}
      }
    }
  }
}
```

### Frontend Query API

```rust
use zenoh::query::ConsolidationMode;

// Query last hour of data for a specific metric
let replies = session
    .get("zensight/sysinfo/server01/cpu/usage")
    .time_range(now - Duration::from_secs(3600)..now)
    .consolidation(ConsolidationMode::None)
    .await?;

while let Ok(reply) = replies.recv_async().await {
    if let Ok(sample) = reply.result() {
        // Process historical sample
    }
}
```

### Implementation Steps

#### 3.1 Storage Deployment
1. Add `zenoh-plugin-storage-manager` to router deployment
2. Configure RocksDB volume for `zensight/**` key expressions
3. Set retention policies (e.g., 7 days, 30 days)

#### 3.2 Frontend Query Integration
1. Add time-range query UI (date picker, presets: 1h, 24h, 7d)
2. Implement query logic in `subscription.rs`
3. Merge historical + live data in charts

#### 3.3 Query Optimizations
1. Add downsampling for large time ranges
2. Implement query pagination for memory efficiency
3. Cache recent queries client-side

### Benefits
- **No bridge changes**: Storage is transparent to publishers
- **Centralized**: Single source of truth for historical data
- **Scalable**: Can add more storage backends as needed
- **Standard API**: Uses Zenoh's native `get()` with time-range selectors

### Dependencies
```toml
# Router plugins (not in bridge/frontend code)
zenoh-plugin-storage-manager = "1.x"
zenoh-backend-rocksdb = "1.x"  # or zenoh-backend-influxdb2
```

---

## Implementation Priority

| Phase | Feature | Effort | Impact | Priority |
|-------|---------|--------|--------|----------|
| 1.1 | Bridge liveliness | Low | High | P0 |
| 1.2 | Device liveliness | Medium | High | P0 |
| 1.3 | Frontend liveliness subscriber | Low | High | P0 |
| 2.1 | AdvancedPublisher with cache | Low | High | P1 |
| 2.2 | AdvancedSubscriber with history | Low | High | P1 |
| 2.3 | Miss detection/recovery | Medium | Medium | P2 |
| 3 | Historical data query (storage) | High | Medium | P3 |

---

## Dependencies

```toml
# For Phase 1 (liveliness)
zenoh = { version = "1.x", features = ["unstable"] }

# For Phase 2 (advanced pub/sub)
zenoh-ext = { version = "1.x", features = ["unstable"] }
```

Note: Liveliness requires the `unstable` feature flag.

---

## Backwards Compatibility

- Phase 1: Run liveliness alongside existing health system, migrate gradually
- Phase 2: AdvancedPublisher is compatible with regular Subscriber (history/recovery just won't work)
- Phase 3: Historical query is additive, doesn't break existing subscriptions

---

## Estimated Timeline

- **Phase 1 (Liveliness)**: Foundation for real-time presence
- **Phase 2 (Advanced Pub/Sub)**: Cache, history, and reliable delivery
- **Phase 3 (Historical Query)**: Long-term storage and time-range queries

Each phase is independently valuable and can be deployed separately.

---

## Open Questions

1. **Liveliness token granularity**: Per-bridge, per-device, or both?
2. **Cache size**: How many samples should publishers cache? Memory vs utility tradeoff.
3. **History depth**: How far back should AdvancedSubscriber request on startup?
4. **Storage retention**: How long to keep historical data? (7 days, 30 days, configurable?)
5. **Downsampling strategy**: For large time ranges, aggregate by minute/hour/day?

---

## Implementation Status

### Phase 1: Liveliness - COMPLETED

- ✅ `LivelinessManager` added to bridge framework (`zensight-bridge-framework/src/liveliness.rs`)
- ✅ `BridgeRunner.with_liveliness()` method for enabling liveliness tokens
- ✅ `BridgeHealth` integration with `with_liveliness()` and async methods
- ✅ Frontend liveliness subscriptions in `subscription.rs`
- ✅ New messages: `BridgeOnline`, `BridgeOffline`, `DeviceOnline`, `DeviceOffline`
- ✅ Unit tests for liveliness key parsing

### Phase 2: Advanced Pub/Sub - COMPLETED

- ✅ `AdvancedPublisherRegistry` added to bridge framework (`zensight-bridge-framework/src/advanced_publisher.rs`)
- ✅ Configurable cache size, miss detection, publisher detection
- ✅ Frontend uses `AdvancedSubscriber` with history and recovery in `subscription.rs`
- ✅ `Protocol::FromStr` implementation for parsing protocol strings
- ✅ zenoh-ext dependency added to workspace

### Phase 3: Historical Query - NOT STARTED

Requires Zenoh storage backend deployment (router configuration).

---

## References

- [Zenoh Liveliness API](https://docs.rs/zenoh/latest/zenoh/liveliness/index.html)
- [Zenoh Session (Query/Queryable)](https://docs.rs/zenoh/latest/zenoh/struct.Session.html)
- [zenoh-ext (AdvancedPublisher/Subscriber)](https://docs.rs/zenoh-ext/latest/zenoh_ext/)
- [Zenoh 1.1.0 Release Notes](https://zenoh.io/blog/2024-12-12-zenoh-firesong-1.1.0/)
