# Plan 05 (enh) — **FOUNDATIONAL**: key-space + control-plane redesign

**Status:** proposal (do this FIRST — it changes the addressing model the other
enh plans build on). **Breaks wire compatibility — explicitly allowed.**
**Crates:** `zensight-common`, `zensight-sensor-core`, every sensor, `zensight`
(frontend), both exporters. **Effort:** M (mechanical breadth, low conceptual risk).

## Why this exists (the two real flaws)

1. **`zensight/**` yields mixed types.** Telemetry, health, errors, alerts,
   commands all live under `zensight/<protocol>/...`, so a `**` subscriber sees
   them all and must guess the type from the key shape (today's giant
   `decode_sample`), and exporters need a `@/`-skip hack (Plan 11). Zenoh's own
   guidance: *a key expr ending in `*` should always yield a single type*
   ([Zenoh abstractions](https://zenoh.io/docs/manual/abstractions/)).
2. **Admin channels collide across hosts.** `zensight/netlink/@/health`,
   `@/commands/expectations`, `@/status/…` are keyed by *protocol only* — two
   hosts running a netlink sensor share (and clobber) the same health/command/
   status keys. There's no per-instance addressing, so a command can't target one
   host vs the fleet (the v1 "in-payload host filter" was a workaround).

Both are structural. Fixing the key space now (compat is breakable) makes the v2
work clean and the platform fleet-ready.

---

## 1. The new key hierarchy

Two top-level **planes**, each type-pure so any wildcard yields one type:

```
zensight/
  telemetry/<protocol>/<source>/<metric…>          # TelemetryPoint        (DATA plane)

  sensor/<host>/<protocol>/                         # CONTROL plane, per instance
    alive                                           #   liveliness token
    health                                          #   HealthSnapshot
    errors                                          #   ErrorReport
    alerts/<alert_key>                              #   Alert (Put=firing/resolved, Delete=tombstone)
    device/<device>/liveness                        #   DeviceLiveness (observed devices)
    status/<topic>                                  #   queryable  (live config/state)
    query/<topic>                                   #   queryable  (on-demand detail, P2)
    cmd/<topic>                                      #   subscriber (commands)

  fleet/<protocol>/cmd/<topic>                       # broadcast a command to ALL instances of a kind

  meta/
    sensor/<host>/<protocol>                         # SensorInfo (discovery/registration)
    correlation/<ip>                                 # CorrelationEntry
```

Design notes, each tied to Zenoh guidance:
- **`<host>` and `<protocol>` are separate chunks** (not `host.protocol`) so a
  single-level `*` selects cleanly — "prefer `robot/12` to `robot12`".
- **`telemetry/` vs `sensor/` vs `meta/`** split makes each subtree type-pure.
- **`source`** (what is observed) stays in telemetry; **`host`** (who reports) keys
  the control plane. For netlink/netring/sysinfo they coincide; for snmp/syslog/
  netflow they differ (sensor host ≠ observed device) — both axes now represented.
- The `@` infix is gone (it borrowed Zenoh's *admin-space* convention confusingly;
  `@`-prefixed keys are reserved by Zenoh for its own admin space).

### Addressing patterns this unlocks
| Want | Key / selector |
|---|---|
| all telemetry | sub `zensight/telemetry/**` (only TelemetryPoint) |
| one protocol's telemetry | sub `zensight/telemetry/netlink/**` |
| all alerts | sub `zensight/sensor/*/*/alerts/**` (only Alert) |
| all health | sub `zensight/sensor/*/*/health` |
| command ONE host | **put** `zensight/sensor/web01/netlink/cmd/expectations` |
| command the FLEET | **put** `zensight/fleet/netlink/cmd/expectations` |
| query ONE host's sockets | **get** `zensight/sensor/web01/netlink/query/sockets?state=listen` |
| query the FLEET's status | **get** `zensight/sensor/*/netlink/status/expectations` (all reply) |

> Commands are `put`s, and you can't `put` to a wildcard — hence the explicit
> `fleet/<protocol>/cmd/<topic>` broadcast key. Each sensor subscribes to **both**
> its own `sensor/<host>/<protocol>/cmd/**` and `fleet/<protocol>/cmd/**`. Queries
> are `get`s, which *can* be wildcard, so fleet-vs-single is just the selector.
> This removes the in-payload host filter entirely.

---

## 2. `zensight-sensor-core`: a reusable `ControlPlane`

Today each sensor hand-rolls its admin channels (netlink's `command.rs`
subscriber+queryable loop; netring would duplicate it). Replace with one library
type so every sensor gets the standard control plane uniformly:

```rust
pub struct ControlPlane { session: Arc<Session>, host: String, protocol: Protocol }

impl ControlPlane {
    pub fn new(session, host, protocol) -> Self;

    // Publishers
    pub fn alert_reporter(&self, format) -> AlertReporter;   // -> sensor/<h>/<p>/alerts/<key>
    pub async fn publish_health(&self, &HealthSnapshot);     // -> sensor/<h>/<p>/health
    pub async fn publish_error(&self, &ErrorReport);         // -> sensor/<h>/<p>/errors
    pub async fn liveliness(&self) -> LivelinessToken;       // -> sensor/<h>/<p>/alive

    // Handlers (spawn their own tasks; subscribe own + fleet key)
    pub fn on_command<T, F>(&self, topic, handler: F)        // cmd/<topic> + fleet/<p>/cmd/<topic>
        where T: DeserializeOwned, F: Fn(T) -> Fut;
    pub fn serve_status<F>(&self, topic, reply: F);          // queryable status/<topic>
    pub fn serve_query<F>(&self, topic, reply: F);           // queryable query/<topic>
}
```

`SensorRunner` constructs the `ControlPlane` from the configured host+protocol and
exposes it. A sensor then writes, e.g.:
```rust
let cp = runner.control_plane();
cp.on_command::<ExpectationCommand, _>("expectations", |cmd| handle.apply(cmd));
cp.serve_status("expectations", || handle.snapshot());
cp.serve_query("sockets", |selector| sockets_detail(selector));   // enh-01 §D
```
The netlink `command.rs` boilerplate and the ad-hoc queryable loops collapse into
these calls. `AlertReporter`'s key builder moves from `zensight/<p>/@/alerts/…`
to `zensight/sensor/<host>/<p>/alerts/…`.

---

## 3. `zensight-common` keyexpr — rewrite the builders

Replace the `@/`-infix builders with the new hierarchy. New `KeyExpr` API
(typed, hard to misuse):
```rust
pub mod key {
    pub fn telemetry(proto, source, metric) -> String;     // zensight/telemetry/…
    pub fn alerts(host, proto, alert_key) -> String;        // zensight/sensor/…/alerts/…
    pub fn health(host, proto) -> String;
    pub fn errors(host, proto) -> String;
    pub fn device_liveness(host, proto, device) -> String;
    pub fn cmd(host, proto, topic) -> String;               // per-host
    pub fn fleet_cmd(proto, topic) -> String;               // broadcast
    pub fn status(host, proto, topic) -> String;
    pub fn query(host, proto, topic) -> String;
    pub fn meta_sensor(host, proto) -> String;
    // selectors (subscriptions)
    pub const ALL_TELEMETRY: &str = "zensight/telemetry/**";
    pub const ALL_ALERTS:    &str = "zensight/sensor/*/*/alerts/**";
    pub const ALL_HEALTH:    &str = "zensight/sensor/*/*/health";
    pub const ALL_ERRORS:    &str = "zensight/sensor/*/*/errors";
    pub const ALL_DEVICE_LIVENESS: &str = "zensight/sensor/*/*/device/*/liveness";
    pub const ALL_ALIVE:     &str = "zensight/sensor/*/*/alive";
    pub const ALL_META_SENSORS: &str = "zensight/meta/sensor/**";
    pub const ALL_CORRELATION:  &str = "zensight/meta/correlation/*";
}
```
Also: consolidate the **duplicate** `HealthSnapshot`/`DeviceLiveness`/`ErrorReport`
types that exist in *both* `zensight-common` and `zensight-sensor-core` into one
(common) — a long-standing smell, cheap to fix while we're here.

---

## 4. Frontend — typed subscriptions, no mega-decoder

Replace the single `zensight/**` subscriber + `decode_sample` guesser with **one
subscriber per type** (each is type-pure, so its decoder can't be wrong, and Zenoh
matches a tighter prefix):
```
sub ALL_TELEMETRY        -> decode TelemetryPoint  -> Message::TelemetryReceived
sub ALL_ALERTS           -> decode Alert (Put) / parse key (Delete) -> AlertReceived/Cleared
sub ALL_HEALTH           -> decode HealthSnapshot
sub ALL_ERRORS           -> decode ErrorReport
sub ALL_DEVICE_LIVENESS  -> decode DeviceLiveness
sub ALL_META_SENSORS     -> decode SensorInfo
sub ALL_CORRELATION      -> decode CorrelationEntry
liveliness ALL_ALIVE     -> SensorOnline/Offline
```
`decode_sample` is deleted; each subscription has a trivial, single-type decoder.
Command/query helpers (`send_command`, `query_json`) target the per-host or fleet
keys from §1. The `parse_alert_cleared` Delete handler parses the new
`sensor/<host>/<proto>/alerts/<key>` shape.

## 5. Exporters — delete the skip-hack

Both exporters now simply `declare_subscriber(ALL_TELEMETRY)` and decode
`TelemetryPoint` unconditionally — **the `is_telemetry_key` guard from Plan 11 is
removed** (the key space guarantees type purity). If/when they export alerts, they
add a *separate* `ALL_ALERTS` subscriber (clean, typed). This is a concrete
simplification the redesign buys.

## 6. `Protocol` extensibility (decouple wire from frontend matches)

Adding a sensor kind today forces editing every exhaustive `match Protocol` in the
frontend (icons, overview, specialized) — friction I hit repeatedly. Two changes:
- The **wire** already uses the lowercase string (`<protocol>` chunk); keep that.
- Give the frontend a **generic fallback** so an unknown protocol degrades to the
  generic device view + a default icon instead of failing to compile. Either add
  `Protocol::Other(String)` (round-trips unknown kinds) **or** make the
  icon/overview matches non-exhaustive with a `_ =>` default. Recommend the `_ =>`
  default for the *display* matches (least churn) and keep `Protocol` a closed enum
  for the *known* kinds — new sensors then need no frontend change to appear.

## 7. Migration (atomic cutover — compat breaks)

All on-bus producers/consumers change together in one release:
1. `zensight-common` key builders + consolidated health types (§3).
2. `sensor-core` `ControlPlane` + `AlertReporter` keys (§2).
3. Each sensor: publish telemetry to `telemetry/…`, wire control plane via
   `ControlPlane` (drop hand-rolled command/queryable code).
4. Frontend typed subscriptions (§4) + command/query key updates.
5. Exporters: subscribe `ALL_TELEMETRY`, drop the guard (§5).
6. Docs/config/justfile/`MEMORY.md` updated; CHANGELOG `BREAKING`.

No schema-version segment (small user base, clean cut). If versioning is ever
wanted, prepend `zensight/v2/…` — note it, don't build it now.

## 8. Testing
- `keyexpr` builders + selector constants (unit; assert exact strings + that each
  `ALL_*` is a valid single-type expr).
- `ControlPlane`: command (per-host + fleet) reaches the handler; status/query
  queryables reply; alerts land on `sensor/<h>/<p>/alerts/*` — integration over an
  in-proc scouting-disabled peer (the v1 AlertReporter test pattern).
- Frontend: each typed subscriber decodes its one type; a fleet `get` to
  `sensor/*/netlink/status/expectations` aggregates replies (simulate 2 peers).
- End-to-end: run netlink + netring + frontend, confirm telemetry/alerts/commands
  flow on the new keys (live netlink unprivileged; netring pcap replay).

## 9. Acceptance criteria
- `zensight/telemetry/**` yields only `TelemetryPoint`; `…/alerts/**` only `Alert`
  (type purity verified).
- Two hosts running the same sensor kind have **distinct, non-colliding** health/
  alert/command/status channels.
- A command targets one host (`sensor/<host>/…/cmd/…`) or the fleet
  (`fleet/<proto>/cmd/…`); the in-payload host filter is gone.
- A fleet status `get` returns one reply per instance.
- Exporters have no `@/` guard and log no decode errors.
- A new (unknown) sensor kind appears in the GUI with the generic view and no
  recompile of exhaustive matches.

---

## 10. Knock-on edits to the other enh plans

Update the key references in enh-01..04 to the new scheme (mechanical):
- `zensight/netlink/@/query/<t>` → `zensight/sensor/<host>/netlink/query/<t>`
- `zensight/netlink/@/commands/<t>` → per-host `sensor/<host>/netlink/cmd/<t>` +
  fleet `fleet/netlink/cmd/<t>`
- `zensight/netring/@/alerts/<k>` → `zensight/sensor/<host>/netring/alerts/<k>`
- netring gets its control plane "for free" via `ControlPlane` (enh-03 §C/§D no
  longer hand-rolls it).
The metric/alert/detector *content* of enh-01..04 is unchanged; only the
addressing moves. Each enh plan carries a one-line pointer to this plan.
