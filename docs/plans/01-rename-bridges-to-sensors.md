# Plan 01 — Rename `bridge` → `sensor`

**Goal:** retire the `bridge` vocabulary (it collides with Zenoh's own meaning and
undersells the crates) in favor of `sensor`, and add room for `sentinel`. Purely
mechanical + rename refactor. **No behavior change. Wire format unchanged.**

**Depends on:** nothing. Do this **first** so new crates are born named right.
**Effort:** S–M (large but mechanical).

---

## 1. Crate renames

| Old crate dir / name | New | Binary name old → new |
|---|---|---|
| `zenoh-bridge-snmp` | `zensight-sensor-snmp` | `zenoh-bridge-snmp` → `zensight-sensor-snmp` |
| `zenoh-bridge-syslog` | `zensight-sensor-syslog` | idem |
| `zenoh-bridge-netflow` | `zensight-sensor-netflow` | idem |
| `zenoh-bridge-modbus` | `zensight-sensor-modbus` | idem |
| `zenoh-bridge-sysinfo` | `zensight-sensor-sysinfo` | idem |
| `zenoh-bridge-gnmi` | `zensight-sensor-gnmi` | idem |
| `zensight-bridge-framework` | `zensight-sensor-core` | (lib) |
| *(exporters unchanged)* | `zensight-exporter-prometheus`, `-otel` | — |

`zensight`, `zensight-common` keep their names.

## 2. Internal type / symbol renames (in `zensight-sensor-core`)

Mechanical find-and-replace, public API of the framework crate:

| Old symbol | New symbol |
|---|---|
| `BridgeRunner<C>` | `SensorRunner<C>` |
| `BridgeConfig` (trait) | `SensorConfig` |
| `BridgeArgs` | `SensorArgs` |
| `BridgeHealth` | `SensorHealth` |
| `BridgeError` / `Result` | `SensorError` / `Result` |
| `BridgeInfo` | `SensorInfo` |
| `BridgeStatus` / `StatusPublisher` | `SensorStatus` / `StatusPublisher` |
| `health::HealthSnapshot.bridge: String` (field) | `.sensor: String` *(see §4 — wire impact)* |
| `BridgeInfo.{name,protocol,key_prefix,…}` | unchanged field names |

**Keep unchanged** (still semantically about *devices*, not bridges):
`DeviceLiveness`, `DeviceStatus`, `LivelinessManager`, `Publisher`,
`AdvancedPublisher*`, `CorrelationRegistry`, `DeviceIdentity`, `CorrelationEntry`,
`ErrorReport`, `ErrorType`.

`zensight_common::BridgeInfo` → rename to `SensorInfo` too (re-exported by
core). `health.rs` doc comments mentioning "bridge" → "sensor".

## 3. Frontend symbol renames (`zensight/src/`)

Cosmetic but do it for consistency (compat already broken):

| Old | New |
|---|---|
| `Message::BridgeOnline(String)` | `Message::SensorOnline(String)` |
| `Message::BridgeOffline(String)` | `Message::SensorOffline(String)` |
| `Message::BridgeInfoReceived(BridgeInfo)` | `Message::SensorInfoReceived(SensorInfo)` |
| `app.ZenSight.bridge_health` | `sensor_health` |
| `app.ZenSight.known_bridges` | `known_sensors` |
| `subscription.rs::parse_bridge_liveliness` | `parse_sensor_liveliness` |
| dashboard "Bridge health summary" UI strings | "Sensor health" |

`view/dashboard.rs` health-summary labels and any user-visible "bridge" string →
"sensor".

## 4. Wire-format decision (IMPORTANT) — full rename, per [INDEX D9](00-INDEX.md)

The rename leaks onto the wire in two places: the `HealthSnapshot.bridge` JSON
field and the `_meta/bridges/<name>` key. Because there are **no third-party wire
consumers** (monorepo, single release), take the clean cut now:

- **Option B (chosen): rename the wire too.** `bridge` field → `sensor`;
  `zensight/_meta/bridges/*` → `zensight/_meta/sensors/*`. Atomic cutover across
  all sensors + the frontend decoder in this same commit. Removes the `bridge`
  vestige entirely.
- **Option A (documented fallback): keep wire stable** via `#[serde(rename =
  "bridge")]` + the old key string. Use only if a staged, mixed-version rollout
  is ever required.

The telemetry prefix `zensight/<protocol>/<source>/<metric>` is **unchanged**
either way — it was never `bridge`-named.

Frontend decoder touch-points for Option B: `subscription.rs::decode_sample`
(`_meta/bridges` → `_meta/sensors`), the `HealthSnapshot`/`SensorInfo` structs,
and `all_bridges_wildcard()` → `all_sensors_wildcard()` in `keyexpr.rs`.

## 5. Mechanical procedure

1. `git mv` each crate dir; update `[package] name`, `[[bin]] name`, and `[lib]`
   in each `Cargo.toml`.
2. Update workspace `Cargo.toml` `members` list and the
   `[workspace.dependencies]` path entries (`zensight-bridge-framework` →
   `zensight-sensor-core`).
3. Workspace-wide symbol rename (ripgrep + sed, then compile-driven fixups):
   - `zensight_bridge_framework` → `zensight_sensor_core` (crate path in `use`).
   - The type renames in §2/§3.
4. Rename config files? **No** — `configs/snmp.json5` etc. stay; only the binary
   that loads them changed name. Update any `--config` paths in docs/docker/
   `flatpak`/`example.yml`/`docker/` compose files that referenced
   `zenoh-bridge-*` binaries.
5. Update docs: root `CLAUDE.md`, each crate `CLAUDE.md`/README, `ARCHITECTURE.md`,
   `README.md`, `CHANGELOG.md` (add a `BREAKING: renamed bridges to sensors`).
6. Update `MEMORY.md` crate map.

## 6. Files that reference bridge binaries (grep targets)

Run before/after to verify nothing is missed:

```bash
rg -l 'zenoh-bridge|bridge_framework|zensight_bridge_framework|BridgeRunner|BridgeConfig|BridgeArgs|BridgeHealth|BridgeError|BridgeInfo|BridgeStatus' \
  --glob '!target'
rg -l 'bridge' docker/ flatpak/ configs/ docs/ *.yml *.md
```

## 7. Acceptance criteria

- `cargo build --workspace` clean; all binaries produce `zensight-sensor-*`.
- `cargo nextest run --workspace` / `cargo test --workspace` green (counts
  unchanged from pre-rename).
- `rg 'zenoh-bridge|BridgeRunner|zensight_bridge_framework'` returns only
  intentional CHANGELOG history lines.
- A running `zensight-sensor-sysinfo` is still discovered by the (renamed)
  frontend exactly as before (wire unchanged — Option A).
- Docker/flatpak build files reference the new binary names.

## 8. Commit

Single commit: `refactor!: rename bridge crates/types to sensor (wire stable)`.
Big diff, zero behavior change — easy to review as a rename.
