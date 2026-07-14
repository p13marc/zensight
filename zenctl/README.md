# zenctl

A bus explorer for the keyspace-v2 convention — `busctl` / `d-feet` / `ros2` for a
ZenSight fleet.

RFC 08 §6 specifies this tool into existence. Every producer MUST serve
`@rpc/<producer>/introspect`, returning the registry slice it was *compiled
against*; the point of that requirement is that "generic explorer tooling — the
`busctl`/`d-feet` equivalent — **needs no compiled-in registry**". `zenctl` is
that tooling.

```bash
cargo run -p zenctl -- node list -c tcp/127.0.0.1:7447
```

## Two halves, kept visibly apart

| | Answers from | Works when the fleet is down | Tells you |
|---|---|---|---|
| **offline** | the compiled-in registry | yes | what *may* exist (declared) |
| **on-bus** | the live fleet | no | what *does* exist (observed) |

The gap between those two is where drift lives, and `doctor` is the command that
reports it. That is why the halves are not blended.

### Offline

```bash
zenctl topic list [--producer netring] [--class telemetry]
zenctl topic info zensight/v1/h-3fa9c2d41b7e/state/netring/alert/abc123
zenctl service list [--producer netlink]
zenctl interface list
zenctl interface show TelemetryPoint
```

`topic info` runs the registry's **parse** direction (RFC 08 §1) — the thing that
replaced positional `split('/')` re-parsing. Variables come back *named*:

```
$ zenctl topic info zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/disk/root/usage_percent
key       zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/disk/root/usage_percent
origin    h-3fa9c2d41b7e
producer  sysinfo
class     telemetry
subject   disk/{mount}/usage_percent
variables
  mount = root
payload   TelemetryPoint
  defined at zensight_common::telemetry::TelemetryPoint
qos       Sampled
cardinality  ~512 keys expected
```

**Declared is not observed.** A pattern with a trailing rest-variable
(`{device}/{path...}`) fixes a *shape*, not its members — the four proxy
producers (snmp/modbus/gnmi/netflow) register that way by design, because their
metric tree belongs to the polled device. `topic list` flags those
`[open-ended]`; `topic echo` is what enumerates them.

### On-bus

```bash
zenctl node list                        # the liveliness roster
zenctl topic echo 'zensight/v1/**'     # subscribe + decode
zenctl service call '*' sysinfo processes --param sort=cpu --param top=5
zenctl service call h-3fa9 netring capture/trigger --body @trigger.json
zenctl doctor                           # fleet vs. this build
```

`node list` is a liveliness query on `zensight/v1/*/state/*/alive` — RFC 04 §5's
"entire fleet-presence protocol, zero payload bytes". The token *key* is the
record. (The RFC took this from rmw_zenoh's `@ros2_lv` discovery space, which is
what `ros2 node list` reads.)

`topic echo` walks wire key → subject → payload type → value with nothing
producer-specific compiled in: the registry binds one payload type per subject
(P5), and the RFC 08 §5 type table (`zensight_common::payload`) turns that name
into a decoder.

## `doctor` — the one `ros2` has no answer for

`ros2 interface show` reads a static type description. `introspect` is served by
the *running binary*, from the same source as its key constants — so it cannot
drift from behavior. RFC 08 §6:

> A disagreement between introspection and the checked-in TOML is a **finding,
> not an ambiguity**: the TOML says what *should* run, the introspection says
> what *does*.

`doctor` fans `introspect` across the fleet and prints those findings:

```
$ zenctl doctor -c tcp/127.0.0.1:7447
✗ h-9706b31ddad3/sysinfo: registry 1.1 (we compiled 1.2)
✗ h-9706b31ddad3/sysinfo: does not serve telemetry thermal/{zone}/temp_celsius
2 finding(s).
```

Version skew, subjects a host serves that we cannot name, subjects we expect that
it does not publish, and hosts still serving a deprecated subject — in one round
trip, without SSH.

(rmw_zenoh puts a type *hash* in the key instead, which makes a schema mismatch
silent non-communication with no operator signal. RFC 10 §3 rejected that
explicitly. This is what we bought with the rejection.)

## Things it will not do, on purpose

- **Silence is never a verdict.** RFC 05 §3.1: an empty reply set conflates an
  offline host, a mistyped origin, and a procedure that is not served. `service
  call` says so rather than guessing; `node list` is what attributes it.
- **Errors are never dressed up as success.** RFC 05 §3: a value reply always
  means success, a failure always rides `reply_err`. An error reply goes to
  stderr with its `error/...` name.
- **No namespace.** RFC 09 §5: debug tools run *without* the session namespace
  and spell full keys — "the honest view of what is on the wire".
- **Scouting is off by default.** A scouting explorer joins whatever mesh it can
  find, which is how a throwaway session ends up talking to a production fleet.
  `--scouting` is opt-in, and you should mean it.
- **Field-level payload schemas.** RFC 01 §5 keeps payload definitions with the
  owning crates; `interface show` points you at the definition rather than
  pretending to reproduce it.

## Fan-in discipline

Every fleet GET goes through one helper (`bus::fleet_get`) because RFC 05 §2.1's
requirements fail *silently* when forgotten:

- **target = All** — the default `BestMatching` short-circuits to a single
  queryable the moment any matching one is declared `complete`, which is "one
  storage config away from silently collapsing the fleet to one reply";
- **consolidation = None** — default consolidation keeps one reply per reply key;
- **attribution by the reply's own concrete key**, never by the key we asked on.

Note `*` in the origin position can never match a verbatim service origin (design
property D4), so `@catalog` is always asked for by name. That is the grammar
working, not an exception to it.
