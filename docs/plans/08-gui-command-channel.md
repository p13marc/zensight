# Plan 08 — GUI command channel + expectations authoring

**Goal:** make the desktop app able to **send commands to sensors** (it currently
cannot — the session is trapped in the subscription stream and the syslog "Apply
to Bridge" button is a dead TODO), then build on that to **author, view, and push
expectations** and **tune anomaly detectors** from the GUI. This is what turns the
sentinel from a config-file feature into an interactive monitoring tool.

**Depends on:** 02 (command primitives), 04 (expectation schema), 07 (alert UI).
**Effort:** M. **Implements [INDEX D8 + D7](00-INDEX.md).**

---

## 1. Foundation: give the app a session handle (fixes the latent gap)

Today `subscription.rs` owns the `Arc<Session>` inside the async stream and yields
a unit `Message::Connected`. Sync `app.update()` therefore can't publish anything.

**Change:**
```rust
// message.rs
Message::Connected(std::sync::Arc<zenoh::Session>),   // was: Connected

// app.rs (ZenSight)
session: Option<std::sync::Arc<zenoh::Session>>,      // new field
// update():
Message::Connected(session) => {
    self.dashboard.connected = true;
    self.session = Some(session);
    // ...
}
Message::Disconnected(_) => { self.session = None; /* ... */ }
```

In `subscription.rs`, after `connect_zenoh` succeeds, `yield
Message::Connected(session.clone())` (the stream keeps its own clone for
subscribing).

> `zenoh::Session` is `Clone`/`Send`/`Sync` and cheap to clone (`Arc` inside);
> `Message` stays `Clone + Send`. ✔

## 2. Shared command primitives — `zensight-common/src/command.rs`

Generalize the syslog `@/commands/<topic>` + `@/status/<topic>` pattern (today
hard-coded in `zenoh-bridge-syslog/src/commands.rs`):

```rust
/// `zensight/<protocol>/@/commands/<topic>`
pub fn command_key(prefix: &str, topic: &str) -> String;
/// `zensight/<protocol>/@/status/<topic>`
pub fn status_key(prefix: &str, topic: &str) -> String;

/// Envelope for any command (correlation id for reply matching, optional).
#[derive(Serialize, Deserialize)]
pub struct Command<T> { pub id: Option<String>, pub body: T }
```

Migrate the syslog bridge to use these (the syslog `FilterCommand` becomes the
`T`). Topics in use: `filter` (syslog), `expectations` (sentinel), `detectors`
(netring).

## 3. App-side command helper (`app.rs`)

```rust
fn send_command<T: Serialize>(&self, protocol: Protocol, topic: &str, cmd: &Command<T>)
    -> Task<Message>
{
    let Some(session) = self.session.clone() else {
        return Task::done(Message::Toast(ToastSeverity::Error, "Not connected".into()));
    };
    let key = command_key(&format!("zensight/{protocol}"), topic);
    let payload = encode(cmd, Format::Json).unwrap();
    Task::future(async move {
        match session.put(&key, payload).await {
            Ok(_)  => Message::CommandAck(topic_owned),
            Err(e) => Message::Toast(ToastSeverity::Error, format!("Command failed: {e}")),
        }
    })
}

/// Query a sensor's status queryable (e.g. current expectation set / eval state).
fn query_status(&self, protocol: Protocol, topic: &str) -> Task<Message> {
    // session.get(status_key).await -> reply -> decode -> Message::StatusReceived(...)
}
```

**This immediately closes the syslog TODO** (app.rs:766 `ApplySyslogFilters`
becomes a real `send_command(Protocol::Syslog, "filter", &cmd)`).

## 4. Expectation command protocol (sentinel side)

The netlink sensor (which embeds the sentinel, Plan 04) gains a command listener,
mirroring the syslog bridge's subscriber+queryable:

```rust
// topic = "expectations"
enum ExpectationCommand {
    SetExpectations(ExpectationsConfig),    // replace the live set
    AddExpectation(ExpectationSpec),        // one entry
    RemoveExpectation { rule: String },
    Reload,                                 // re-read the on-disk config file
}
// status queryable replies with:
struct ExpectationStatus {
    expectations: Vec<ExpectationView>,     // rule, kind, severity, for_secs, current_state
    last_eval_ms: i64,
    firing: Vec<String>,                    // alert_keys currently firing
}
```

`Evaluator` (Plan 04) gains `replace_expectations(...)`/`add`/`remove` behind its
`RwLock`, so the GUI can hot-swap the live set without restarting the sensor.

**Persistence:** a pushed `SetExpectations` updates the *running* set immediately;
the sensor also writes it back to its config file (or a sidecar
`expectations.runtime.json5`) so it survives restart. Document the precedence
(runtime overlay > file) clearly.

## 5. GUI: Expectations view

A new top-level view (add `CurrentView::Expectations`) + sidebar entry. Layout
modeled on the existing Alerts view + the syslog filter panel:

- **Sensor picker:** dropdown of discovered netlink sensors (from
  `known_sensors`). On select, `query_status(Netlink, "expectations")` populates
  the panel.
- **Expectation list:** each row shows rule, type (socket/link/route/…), severity,
  `for:`, and live state (✓ satisfied / ⚠ firing / ? unknown) from the status
  reply + the `external` alerts map.
- **Add/edit form** per expectation family (socket listening, established-to,
  link-up, route, neighbor, wireguard, diagnostics) — typed inputs, not raw
  JSON, with a "raw JSON5" escape hatch.
- **Actions:** Add → `AddExpectation`; Remove → `RemoveExpectation`; "Apply all"
  → `SetExpectations`; "Reload from file" → `Reload`.
- Reuses `SettingsState`-style form fields; new `ExpectationsState` struct holds
  the form + the fetched status (same shape as `SyslogFilterState`).

Messages (message.rs):
```rust
OpenExpectations, SelectExpectationSensor(String),
ExpectationStatusReceived(String, ExpectationStatus),
AddExpectation, RemoveExpectationRule(String), ApplyExpectations, ReloadExpectations,
// + per-field setters for the add/edit form
```

## 6. Promote a GUI threshold-rule to a server-side expectation (D7)

In the Alerts view, a local `AlertRule` (e.g. `cpu/usage > 90`) gets a **"Promote
to sensor"** button → builds a `metric-threshold` expectation and
`AddExpectation`s it to the relevant sensor, so it fires headless thereafter.
(Add a `MetricThreshold` expectation family to Plan 04's catalog: evaluate a
named telemetry metric the sensor already produces against an operator/threshold.)

## 7. Detector tuning (netring, topic = "detectors")

Same machinery: `DetectorCommand::{Enable, Disable, Tune{detector, params}}` to
`zensight/netring/@/commands/detectors`; status replies with the active detector
set + per-detector counts. A small panel in the netring specialized view (Plan
09) or the Expectations view drives it.

## 8. Security note (do not skip)

The command channel is **remote control of hosts**. Even though every command here
is advisory (expectations are read-only checks; detectors only observe), pushing
config to a fleet is sensitive. Document:
- Scope command keys under access-controlled Zenoh (ACL/auth) in production.
- Sensors should validate + bound incoming commands (e.g. cap expectation count,
  reject malformed CIDRs) exactly like config-file validation.
- Consider a config flag `accept_remote_commands: bool` (default `true` on
  trusted networks, overridable) per sensor.

## 9. Tests
- `command.rs` key builders + `Command<T>` round-trip.
- Sentinel command handler: `SetExpectations` swaps the live set; status reply
  reflects it (unit test against the `Evaluator` with a fake reporter).
- App: `send_command` builds the right key/payload (inject a fake session that
  records puts); `Promote` builds a correct expectation.
- Simulator: Expectations view renders a fetched status; Add emits `AddExpectation`.

## 10. Acceptance criteria
- The syslog "Apply to Bridge" button actually pushes a filter command (TODO
  closed) and the bridge applies it live.
- From the GUI, a user adds `socket: sshd listen 22` to a running netlink sensor;
  stopping sshd raises the expectation alert (Plan 04) **with the GUI closed and
  reopened** (proves it's server-side, not GUI-local).
- The Expectations view shows live satisfied/firing state per expectation.
- Promoting a CPU-threshold rule makes it fire headlessly.
- Command + view unit/simulator tests green; docs updated.
