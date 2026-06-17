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

## 1b. Delivery semantics: late-joiner **state** vs **events** (Advanced Pub/Sub)

The key design must encode *recoverability*, because a GUI/exporter that connects
**after** a value was published still needs the current state. Zenoh's
`zenoh-ext` `AdvancedPublisher`/`AdvancedSubscriber` exist exactly for this
(cache + history → late joiners pull recent samples; sample-miss-detection +
recovery → no dropped samples). ZenSight already half-uses them: the frontend
subscribes with `.history(HistoryConfig::default().detect_late_publishers())
.recovery(RecoveryConfig::default())`, and `sensor-core` has an
`AdvancedPublisherRegistry` (`CacheConfig::max_samples`, `MissDetectionConfig`,
`publisher_detection`).

> **The bug this fixes.** `AlertReporter` and `Publisher` currently publish with
> **plain `session.put`** — *not* the AdvancedPublisher. So there is **no cache**
> for the frontend's `history()` to retrieve. Telemetry self-heals on the next
> poll, but **alerts only publish on state change**, so a GUI opened after an
> alert fired **never sees the firing alert**. The redesign makes every *state*
> channel late-joiner-recoverable.

**The rule:** anything representing *current state* must be retrievable by a late
joiner — via a **cached `AdvancedPublisher`** (push) **or a queryable** (pull).
**Commands are events** and must be **cacheless** (never replay stale config).

| Channel | Semantics | Delivery |
|---|---|---|
| `telemetry/…` (aggregates) | state (latest matters) | **AdvancedPublisher**, cache last 1 per key (small N for sparklines); subscriber `history()` → current values on connect |
| `…/alerts/<key>` | state (firing set must survive late join) | **AdvancedPublisher** + cache (latest per `alert_key`) + `sample_miss_detection`+heartbeat; subscriber `history()`+`recovery()` |
| `…/health`, `…/errors`, `meta/sensor/…` | state (current snapshot) | AdvancedPublisher, cache last 1 per key; subscriber `history()` |
| `…/status/<topic>`, `…/query/<topic>` | pull state | **queryable** — inherently late-joiner-safe (no cache needed) |
| `…/cmd/<topic>`, `fleet/…/cmd/…` | **events** | **plain** publisher/subscriber — **no cache/history** (replaying old commands would re-apply stale config) |
| `…/alive` | state | Zenoh **liveliness** token + initial `get` |

Two subtleties to bake in:
- **Alert cache + tombstones.** A resolved alert is `Put(resolved)`→`Delete`; the
  cache's latest sample for that key becomes the `Delete`, so a late joiner's
  `history()` sees the cleared state (not a stale firing). **Verify** `zenoh-ext`
  serves `Delete` samples in history; if not, fall back to a **`query/alerts`
  queryable** the GUI `get`s on connect to seed the firing set (authoritative,
  bulletproof), then the live subscription keeps it current. Recommend shipping
  the `query/alerts` seed regardless — it's cheap and removes any doubt.
- **Cache stays bounded by P2.** Cache size is *per key*; because we stream only
  low-cardinality aggregates (never per-flow/per-socket — P2), the number of keys
  (and thus total cached samples) is bounded. The cardinality discipline is what
  makes caching affordable.

The `ControlPlane` (§2) is where this is enforced: it hands out
AdvancedPublishers for the state channels and plain publishers for commands, with
sensible per-channel cache/recovery defaults — so individual sensors can't get it
wrong.

---

## 2. `zensight-sensor-core`: a reusable `ControlPlane`

Today each sensor hand-rolls its admin channels (netlink's `command.rs`
subscriber+queryable loop; netring would duplicate it). Replace with one library
type so every sensor gets the standard control plane uniformly:

Per §1b, the control plane hands out **AdvancedPublishers (cached)** for state
channels and **plain** publishers for command events — so a sensor can't
accidentally publish alerts/telemetry without late-joiner recovery:

```rust
pub struct ControlPlane { session: Arc<Session>, host: String, protocol: Protocol }

impl ControlPlane {
    pub fn new(session, host, protocol) -> Self;

    // State publishers — AdvancedPublisher w/ cache + miss-detection (§1b)
    pub fn telemetry_publisher(&self) -> Publisher;          // AdvancedPub, cache N -> telemetry/<p>/…
    pub fn alert_reporter(&self, format) -> AlertReporter;   // AdvancedPub, cache+heartbeat -> …/alerts/<key>
    pub async fn publish_health(&self, &HealthSnapshot);     // AdvancedPub, cache 1 -> …/health
    pub async fn publish_error(&self, &ErrorReport);         // AdvancedPub, cache N -> …/errors
    pub async fn liveliness(&self) -> LivelinessToken;       // …/alive

    // Pull state — queryables (late-joiner-safe by construction)
    pub fn serve_status<F>(&self, topic, reply: F);          // queryable status/<topic>
    pub fn serve_query<F>(&self, topic, reply: F);           // queryable query/<topic>
    pub fn serve_alerts_query(&self, reporter: &AlertReporter); // query/alerts seed (§1b fallback)

    // Events — PLAIN sub (no history), own + fleet key
    pub fn on_command<T, F>(&self, topic, handler: F)        // cmd/<topic> + fleet/<p>/cmd/<topic>
        where T: DeserializeOwned, F: Fn(T) -> Fut;
}
```
`AlertReporter` is refactored to publish via the cached AdvancedPublisher (fixing
the §1b bug) and to expose its firing set so `serve_alerts_query` can seed late
joiners. The existing `AdvancedPublisherRegistry` is the implementation vehicle.

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
`AdvancedSubscriber` per type** (each is type-pure, so its decoder can't be wrong,
Zenoh matches a tighter prefix, and each gets the right late-joiner config per
§1b):
```
sub ALL_TELEMETRY        history()                 -> TelemetryPoint -> TelemetryReceived
sub ALL_ALERTS           history()+recovery()      -> Alert(Put)/key(Delete) -> AlertReceived/Cleared
sub ALL_HEALTH           history()                 -> HealthSnapshot
sub ALL_ERRORS           history()                 -> ErrorReport
sub ALL_DEVICE_LIVENESS  history()                 -> DeviceLiveness
sub ALL_META_SENSORS     history()                 -> SensorInfo
sub ALL_CORRELATION      history()                 -> CorrelationEntry
liveliness ALL_ALIVE     + initial get             -> SensorOnline/Offline
```
- **On connect**, `history()` pulls the current value of every cached state key —
  so the dashboard shows current telemetry + the **firing-alert set** immediately,
  not after the next poll/state-change (the §1b fix, observed from the consumer).
- The **alerts** subscriber adds `recovery()` so a missed firing/resolved is
  re-fetched. As a belt-and-suspenders seed (§1b), on connect the app also `get`s
  `sensor/*/*/query/alerts` to populate `AlertsState.external` authoritatively.
- `decode_sample` is deleted; each subscription has a trivial, single-type decoder.
  Command/query helpers (`send_command`, `query_json`) target the per-host/fleet
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
- **Late-joiner (the §1b fix):** fire an alert, *then* start a fresh subscriber
  with `history()` → it receives the firing alert (and the current telemetry
  values) without waiting for the next state change/poll. Resolve it → the late
  subscriber sees the cleared state (via cached `Delete` or the `query/alerts`
  seed). This is the test that would have caught the current bug.

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
- **A GUI opened *after* an alert fired shows that alert immediately** (cached
  AdvancedPublisher + `history()` / `query/alerts` seed), and shows current
  telemetry values on connect rather than a blank panel until the next poll.

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
