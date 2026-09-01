# probe — configuration

`configs/probe.json5` ships with **every example target commented out**. That is
deliberate: no generator can invent a URL worth watching, and a probe pointed
somewhere by default would be a monitoring tool making requests nobody asked
for. An empty target list is valid — the sensor says so at startup rather than
looking broken — and is what CI runs.

## Keys

| Key | Default | Note |
|---|---|---|
| `probe.vantage` | the hostname | **where this sensor looks from.** Rides on every result and every alert |
| `probe.source` | `vantage` | the reporting host every series and alert is filed under. A result is an observation made *from somewhere*: filing it under the target would make two hosts probing the same URL collide on one identity (#883). The target rides in the key path and in the `probe`/`target`/`kind` labels |
| `probe.interval_secs` | `60` | per-target override; floored at 5 s |
| `probe.timeout_secs` | `10` | per-target override; **must be shorter than that target's interval** |
| `probe.max_concurrent` | `8` | checks in flight at once, across all targets |
| `probe.targets` | `[]` | see below |
| `probe.alerts.*` | see [`assertions.md`](assertions.md) | |

## Per target

| Key | Applies to | Note |
|---|---|---|
| `name` | all | the device slug and the alert key; **must be unique** |
| `kind` | all | `http` · `tls` · `dns` · `tcp` · `icmp` · `certfile` |
| `target` | all | a URL, `host:port`, a name, or an absolute path — checked against the kind at startup |
| `expect_status` | http | empty = any 2xx |
| `expect_body` | http | a literal substring |
| `allow_offhost_redirect` | http | **false** by default |
| `server_name` | tls, certfile | SNI, and the name matched against SANs |
| `inspect_untrusted` | tls | true by default — report a bad certificate instead of erroring |
| `resolver` | dns | `ip[:port]`; default is the system resolver, and either way it is **named in the result** |
| `expect_addrs` | dns | addresses the answer must contain |
| `enabled` | all | skip without deleting |

## The bounds are checked, not hoped for

Startup refuses:

- a timeout at or above its target's interval — *"a check that can outlive its
  own tick is queued, not bounded"*;
- a duplicate target name — names are the device slug and the alert key, so a
  duplicate silently overwrites another target's series;
- a target whose shape does not match its kind (a URL-less `http`, a portless
  `tls`, a relative `certfile` path);
- an `icmp` target in a build without the `icmp` feature, **naming the `tcp`
  alternative** rather than letting the check silently never run.

Intervals are floored at 5 s regardless of what a target asks for.

## ICMP

ICMP needs a raw socket. It follows the repo's convention for
capability-needing collectors: a **build feature**, off by default
(`cargo build --features icmp`), plus a target in the config. A default build
is unprivileged and refuses an `icmp` target at startup.

For most services a `tcp` probe answers the same operational question and needs
nothing. The shipped systemd unit carries the `CAP_NET_RAW` lines commented
out, next to that sentence.

## Certificate files under a hardened unit

`kind: "certfile"` reads a PEM off disk. The shipped systemd unit uses
`ProtectSystem=strict`, which hides everything not listed, so each directory
holding a watched certificate needs its own `ReadOnlyPaths=` line. The unit
ships with one commented example.

## Where to run it

**In more than one place, on purpose.** On the edge for the outside view; on a
guest for the hairpin view — the check that would have caught 2026-08-20; on an
operator's workstation for the real user's view. Same sensor, same target list
if you like, different `vantage`, and the disagreement between them is the
information.
