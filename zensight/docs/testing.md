# Frontend UI testing

This is the authoritative guide for testing the ZenSight frontend, using Iced
0.14's testing framework.

## Overview

The frontend uses two complementary testing approaches:

1. **Simulator tests** — headless unit tests using `iced_test::Simulator`.
2. **Tester (F12)** — interactive E2E recording and playback.

## Quick start

```bash
# Run all UI tests
cargo test -p zensight

# Run a specific test
cargo test -p zensight test_dashboard_empty

# Build with E2E recording enabled
cargo run -p zensight --features tester
```

## Simulator tests

The `iced_test` crate provides a `Simulator` that renders UI components in
memory without creating a window. This is why view functions are kept pure
(state in, widgets out) — they can be rendered and asserted against directly.

### Basic usage

```rust
use iced_test::simulator;
use zensight::view::dashboard::{DashboardState, dashboard_view};
use zensight::message::Message;

#[test]
fn test_dashboard_empty() {
    // Create component state
    let state = DashboardState::default();

    // Create simulator from view function
    let mut ui = simulator(dashboard_view(&state));

    // Find elements by text content
    assert!(ui.find("Waiting for telemetry data...").is_ok());
}
```

### Selectors

The `Simulator` uses selectors to find and interact with elements. The most
common selector is `&str`, which matches by text content:

```rust
ui.find("Settings")           // Finds element containing "Settings"
ui.find("router01")           // Finds element containing "router01"
ui.find("5 metrics")          // Finds element containing "5 metrics"
```

Other selector types:
- `widget::Id` — find by widget ID
- `Point` — find by screen coordinates
- Custom closures implementing the `Selector` trait

### Interactions

#### Click

```rust
let result = ui.click("Settings");
assert!(result.is_ok());

// The click may fail if the element is not found or not visible
match ui.click("NonExistent") {
    Ok(_) => panic!("Should not find this"),
    Err(e) => println!("Expected error: {:?}", e),
}
```

#### Type text

```rust
// Type text into the focused input
ui.typewrite("router01");

// Tap a specific key
use iced::keyboard::Key;
ui.tap_key(Key::Named(iced::keyboard::key::Named::Enter));
```

### Checking messages

After interactions, retrieve the messages produced:

```rust
#[test]
fn test_settings_button() {
    let state = DashboardState::default();
    let mut ui = simulator(dashboard_view(&state));

    let _ = ui.click("Settings");

    let messages: Vec<Message> = ui.into_messages().collect();

    assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));
}
```

### Testing with mock data

Use the `zensight::mock` module to generate realistic test data:

```rust
use zensight::mock;
use zensight::view::device::{DeviceDetailState, device_view};
use zensight::message::DeviceId;
use zensight_common::Protocol;

#[test]
fn test_device_with_metrics() {
    let device_id = DeviceId {
        protocol: Protocol::Sysinfo,
        source: "server01".to_string(),
    };
    let mut state = DeviceDetailState::new(device_id);

    for point in mock::sysinfo::host("server01") {
        state.update(point);
    }

    let mut ui = simulator(device_view(&state));

    assert!(ui.find("cpu/usage").is_ok());
    assert!(ui.find("memory/used").is_ok());
}
```

### Available mock generators

| Function | Description | Metrics generated |
|----------|-------------|-------------------|
| `mock::snmp::router(name)` | SNMP router | sysUpTime, sysName, ifInOctets, ifOutOctets |
| `mock::snmp::switch(name, ports)` | SNMP switch | Per-port interface metrics |
| `mock::sysinfo::host(name)` | System metrics | cpu/usage, memory/used, disk/usage, network/rx_bytes |
| `mock::syslog::messages(host)` | Syslog messages | Various facilities and severities |
| `mock::modbus::plc(name)` | Modbus PLC | holding/temperature, coil/running, input/pressure |
| `mock::mock_environment()` | Full environment | All of the above combined |

### Snapshot testing

Take visual snapshots for regression testing:

```rust
use iced::Theme;

#[test]
fn test_dashboard_snapshot() {
    let state = DashboardState::default();
    let mut ui = simulator(dashboard_view(&state));

    let snapshot = ui.snapshot(&Theme::Dark).unwrap();
    assert!(snapshot.matches_image("snapshots/dashboard_empty.png").unwrap());
}
```

Snapshots are saved on first run and compared on subsequent runs.

## E2E recording with the tester (F12)

The `tester` feature enables an interactive developer tool for recording UI
tests.

### Enabling the tester

```bash
cargo build -p zensight --features tester
cargo run   -p zensight --features tester
```

### Using the tester panel

1. **Open**: press **F12** to toggle the tester panel.
2. **Record**: click "Record" to start recording interactions.
3. **Interact**: use the application normally — clicks, typing, etc. are recorded.
4. **Stop**: click "Stop" to end recording.
5. **Save**: save the recording as an `.ice` file.

### `.ice` file format

Recorded tests are saved as `.ice` files with a simple text format:

```
# Test: Dashboard navigation
preset: default
viewport: 1024x768

click "Settings"
wait 100
find "Zenoh Connection"
click "Back"
wait 100
find "Dashboard"
```

### Running `.ice` tests

Use `iced_test::run()` to execute `.ice` files:

```rust
use iced_test::run;
use zensight::ZenSight;

fn main() -> Result<(), iced_test::Error> {
    run(ZenSight::default(), "tests/ice/")
}
```

### Presets

Define application presets for reproducible test environments:

```rust
impl iced::program::Program for ZenSight {
    fn presets(&self) -> &[iced::program::Preset<Self>] {
        &[
            Preset::new("empty", || ZenSight::default()),
            Preset::new("with_devices", || {
                let mut app = ZenSight::default();
                // Add mock devices
                app
            }),
        ]
    }
}
```

Reference presets in `.ice` files:

```
preset: with_devices
click "router01"
find "Device Details"
```

## Test organization

### File structure

```
zensight/
├── src/
│   ├── lib.rs           # Exposes modules for testing
│   ├── mock.rs          # Mock data generators
│   ├── replay.rs        # .zrec capture replay (#747)
│   └── view/            # View components to test
└── tests/
    ├── ui_tests.rs      # Simulator-based tests
    ├── zrec_fixtures.rs # Captured-traffic decode fixtures (#747)
    ├── fixtures/zrec/   # The recorded corpus (see below)
    └── ice/             # E2E test recordings (optional)
        ├── navigation.ice
        └── settings.ice
```

### Test categories

| Category | File | Description |
|----------|------|-------------|
| Dashboard | `ui_tests.rs` | Empty state, device cards, navigation buttons |
| Device | `ui_tests.rs` | Metrics display, back button, filtering |
| Settings | `ui_tests.rs` | Form rendering, save functionality |
| Alerts | `ui_tests.rs` | Alert rules, acknowledgment |

## Replay fixtures (`.zrec`) — #747

A hand-built test sample encodes what we *think* a sensor publishes; a `.zrec`
capture encodes what one *did*. Every wire-shape bug we have found — the
unstamped `@media` samples, the `express` disagreement, the `FrameMeta` twin
question — is the gap between those two. The corpus in `tests/fixtures/zrec/`
closes it for the decode path.

A `.zrec` is newline-delimited JSON in zenkey-fleet's tape dialect (RFC 09
§5.2, now RFC 13 §4): line 1 is a header stating what was watched, every
further line is a sample row (`"bytes"` is the lossless payload; `"value"` is
a rendering and never round-trips) or a `{"dropped": n}` record placed where
the capture itself lost samples.

### The corpus

| File | Selector | What it pins |
|------|----------|--------------|
| `sysinfo-telemetry.zrec` | `v1/*/telemetry/sysinfo/**` | the bulk telemetry decode → `Reading` |
| `state-plane.zrec` | `v1/*/state/**` | health / sensor-doc / evidence routing |
| `catalog-entities.zrec` | `v1/@catalog/state/**` | the correlator's `HostEntity` (a `v1/*` selector can never see `@catalog` — grammar D4) |

Consumed by two test layers:

- `tests/zrec_fixtures.rs` — headers, per-family decode coverage, and the
  lossless round trip (row → `replay::sample_view` → `ZrecWriter` →
  `ZrecReader` → identical row).
- `src/app.rs` `mod zrec_replay_tests` — the whole capture folded through
  `App::update`, twice, with identical derived state (a replay folds no live
  time).

The `zensight::replay` module is the seam: `load`/`read` a capture,
`decode_row` (routes tombstones and strips the header's base the way the live
session's namespace does), `messages()` for an update-fold, and
`sample_view()` to lift a row into a `zenkey_fleet::SampleView` for
`MonitorCore::ingest_at`-style consumers. It is deliberately session-less —
the RFC 09 §5.2 *pane replay* posture: nothing in it can publish. Replaying
onto a bus is `zenkey_fleet::replay()`'s job and carries the re-stamping /
retire-gate / synthetic-marker obligations (RFC 09 §5.3) with it.

### The assertion policy (and the regeneration contract)

Fixture tests assert only **capture-stable facts**: which key families
decode, which message variants appear, counts derived from the file itself.
Never a hostname, an origin hash, a metric value, or a timestamp. Never feed
`Message::Tick` or assert staleness in a fold test — those read the wall
clock. The contract this buys:

> Re-run `scripts/record-fixtures.sh`, and `cargo test -p zensight` must pass
> unchanged.

The script stands up the same isolated deployment as
`scripts/conformance-verify.sh` (loopback rendezvous, multicast off, sysinfo
+ logs + systemd + correlator) and records the three captures with
`zensight-conformance --record-zrec`. It is **manual, never CI** — the corpus
is a pinned regression input, not weather. Recorder start order matters and
is documented in the script: the captures open before the correlator starts,
because entity docs publish once (on evidence arrival), not on a cadence.

An exact-value assertion on fixture content is a reviewable violation of this
section. Synthetic traffic / fault injection (`zenkey_fleet::Synth`) is
deliberately deferred: it sits behind the fleet `decode` feature, which the
GUI build keeps off.

## Best practices

### 1. Test view functions independently

```rust
// Good: test the individual view
let mut ui = simulator(dashboard_view(&state));

// Avoid: testing through the full application (slower, more fragile)
```

### 2. Use descriptive state setup

```rust
#[test]
fn test_device_with_warning_status() {
    let mut state = DeviceDetailState::new(device_id);
    state.is_healthy = false;                          // Explicit warning state
    state.last_seen = now - Duration::from_secs(120);  // Stale data

    let mut ui = simulator(device_view(&state));
    assert!(ui.find("Warning").is_ok());
}
```

### 3. Check specific messages

```rust
// Good: check for a specific message variant
assert!(messages.iter().any(|m| matches!(m, Message::OpenSettings)));

// Better: check message content when applicable
assert!(messages.iter().any(|m| matches!(
    m,
    Message::SelectDevice(id) if id.source == "router01"
)));
```

### 4. Test error and edge states

```rust
#[test]
fn test_empty_metrics_list() {
    let state = DeviceDetailState::new(device_id);
    let mut ui = simulator(device_view(&state));
    assert!(ui.find("No metrics available").is_ok());
}

#[test]
fn test_disconnected_state() {
    let mut state = DashboardState::default();
    state.connected = false;
    let mut ui = simulator(dashboard_view(&state));
    assert!(ui.find("Disconnected").is_ok());
}
```

### 5. Use the mock environment for integration

```rust
#[test]
fn test_multi_protocol_dashboard() {
    let mut state = DashboardState::default();

    for point in mock::mock_environment() {
        state.process_telemetry(point);
    }

    let mut ui = simulator(dashboard_view(&state));

    assert!(ui.find("router01").is_ok());  // SNMP
    assert!(ui.find("server01").is_ok());  // Sysinfo
    assert!(ui.find("plc01").is_ok());     // Modbus
}
```

## Troubleshooting

### Test fails to find an element

```
Error: SelectorNotFound { selector: "text == \"Settings\"" }
```

**Causes:** text doesn't match exactly (spacing, case); the element isn't
rendered in the current state; the element is hidden or off-screen.

**Solutions:** use `ui.snapshot()` to visually inspect the rendered UI; check the
view function to verify the element is included; verify the state is set up
correctly.

### Message not produced

```rust
let messages: Vec<Message> = ui.into_messages().collect();
assert!(messages.is_empty());  // Unexpected!
```

**Causes:** the click target wasn't a button; the button's `on_press` is `None`;
the element wasn't found.

**Solutions:** check that `ui.click()` returns `Ok`; verify the button has an
`on_press` handler; use `ui.find()` first to confirm the element exists.

### Snapshot mismatch

```
assertion failed: snapshot.matches_image("snapshots/test.png")
```

**Causes:** intentional UI changes (update the snapshot); font-rendering
differences across platforms; timing-dependent content.

**Solutions:** delete the old snapshot to regenerate; use hash-based comparison
for cross-platform tests; mock time-dependent values.

## API reference

### Simulator methods

| Method | Description |
|--------|-------------|
| `find(selector)` | Find an element, returns `Result<Output, Error>` |
| `click(selector)` | Click an element by selector |
| `point_at(position)` | Move the cursor to a position |
| `tap_key(key)` | Press and release a key |
| `typewrite(text)` | Type text character by character |
| `simulate(events)` | Send raw events |
| `snapshot(theme)` | Take a visual snapshot |
| `into_messages()` | Get an iterator of produced messages |

### Error types

| Error | Description |
|-------|-------------|
| `SelectorNotFound` | No element matches the selector |
| `TargetNotVisible` | Element found but not visible |

## If the `zensight` tests segfault

On a headless Linux box with Mesa installed, `cargo test -p zensight` segfaults
intermittently with **no output at all**: the process dies before libtest prints a
result line, so there is no `FAILED` and no panic message to grep for. It looks like
this and nothing else:

```
error: test failed, to rerun pass `-p zensight --lib`      # or --test ui_tests
Caused by:
  process didn't exit successfully: … (signal: 11, SIGSEGV: invalid memory reference)
```

It is not your change. `iced_test::simulator` stands up a real **wgpu** device;
wgpu picks the Vulkan backend; on a machine with no GPU that resolves to
**lavapipe**, Mesa's software Vulkan; and many tests doing that concurrently
crash inside the Vulkan loader. The faulting frame is in `libvulkan.so.1`, under
`wgpu_core::snatch::SnatchLock` (#687).

**It is not only `ui_tests`.** That is what this section originally said, and it
sent at least one person looking for a real bug in the other half. The crate's
own **lib** tests take the same path and crash *more* often:

| target | default | `WGPU_BACKEND=gl` |
|---|---|---|
| `--test ui_tests` | 6 crashes / 40 runs | 0 / 40 |
| `--lib` | **3 crashes / 10 runs** | 0 / 10 |

(measured on `master` at `3f8083b`, interleaved so machine load was controlled)

Force wgpu off Vulkan and it goes away. **Since #829 the test binaries do that
themselves**: a pre-main `#[ctor]` guard sets `WGPU_BACKEND=gl` unless the
caller already set one — once in `src/lib.rs` for the `--lib` target, once at
the top of `tests/ui_tests.rs`, because each test binary needs its own. A plain
`cargo test -p zensight` is safe on any host, and an explicit `WGPU_BACKEND`
from the environment still wins.

The recipe remains, as the discoverable name for the same thing (and the place
`just --list` carries this story):

```bash
just test-ui                    # WGPU_BACKEND=gl cargo test -p zensight
just test-ui --lib              # takes the usual target selection…
just test-ui test_dashboard     # …and the usual filters and flags
```

Deliberately **not** set in `.cargo/config.toml`: that file's `[env]` block
applies to `cargo run` as well, and downgrading the real GUI's renderer on every
developer machine to fix a test-only problem is the wrong trade. The guard lives
in the test binaries precisely because that is the only scope where "always gl"
is correct.

If you add a **new** integration-test target that builds simulators, copy the
guard from the top of `tests/ui_tests.rs` — a target without it is back to the
coin flip.

CI is unaffected — the runner image ships no Vulkan ICD, so wgpu never takes
this path there. Which also means a red `test` job on CI is **not** this, and
should be read as a real failure.

## See also

- [Iced Testing PR #3059](https://github.com/iced-rs/iced/pull/3059) — original testing framework implementation.
- [iced_test crate documentation](https://docs.rs/iced_test)
- [`views.md`](views.md) — the view/state pattern the Simulator exercises.
- [`../README.md`](../README.md) — frontend overview.
