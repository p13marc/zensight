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
| `expect_body` | http | a literal substring, looked for in the first 256 KiB of the body. Without it the body is not read at all — a plain up/down check on an endpoint serving a large response must not buffer it inside a `MemoryMax=64M` unit |
| `follow_redirects` | http | true by default; **false** means the 3xx itself is the answer, so `expect_status: [200]` on an endpoint that starts sending you to a login page reports down. Read by nothing until #1134 |
| `allow_offhost_redirect` | http | **false** by default. Every hop is checked, not only the last — a chain that leaves the host and comes back has still left it |
| `server_name` | tls, certfile | SNI, and the name matched against SANs |
| `inspect_untrusted` | tls | true by default — report a bad certificate instead of erroring |
| `ca_file` | tls, http | a PEM of extra trust anchors for **this target** (#1136). **Added to** the public roots, not replacing them — a target behind an internal CA still needs them for anything in its redirect chain. A local path, and it is never on the wire: see below |
| `client_cert_file` / `client_key_file` | tls, http | a client chain and its key, for an mTLS endpoint (#1136). Both or neither; one without the other is refused at startup. The key may be PKCS#8, PKCS#1 or SEC1 |
| `chain_invalid_alert` | tls | per-target override for `alerts.chain_invalid` (#1136). `false` exempts **this target alone**. Prefer `ca_file`: this says "I know this chain does not validate and I accept it", which is a weaker statement than naming the CA |
| `resolver` | dns | `ip[:port]`; default is the system resolver, and either way it is **named in the result** |
| `expect_addrs` | dns | the answer must contain **at least one** of them. It required *all* until #1134, which made a round-robin name with two A records a permanent critical: the resolver hands back one |
| `enabled` | all | skip without deleting |

## Trust material is file-only, and never on the wire

`ca_file`, `client_cert_file`, `client_key_file` and `chain_invalid_alert` are
**not fields of the wire target type** (#1136). A target set pushed over
`@rpc/probe/targets/set` or arriving on `@desired` has nowhere to carry them,
so no fleet controller can introduce, change or remove a trust anchor.

That is a structural defence rather than a lint. A controller that could hand a
sensor a new CA could hand it *any* CA, and the sensor would then believe
whatever that CA signed — which is a categorically larger power than telling it
which URL to check.

The consequence, stated plainly: a target that arrives in a **push** inherits
the trust material of the same-named target in the file, and has none if there
is no such target. It fails closed.

Before this, the root store was `webpki_roots` and nothing else, so **every
internal-CA endpoint was a permanent critical** — including the ZenSight mesh
certificates this crate exists to watch, which are signed by an internal CA by
definition. The only escape was `alerts.chain_invalid: false`, which is global.

## The bounds are checked, not hoped for

Startup refuses:

- a timeout at or above its target's interval — *"a check that can outlive its
  own tick is queued, not bounded"*;
- a duplicate target name — names are the device slug and the alert key, so a
  duplicate silently overwrites another target's series;
- a `ca_file`, `client_cert_file` or `client_key_file` that does not exist —
  a trust anchor the sensor cannot read is a handshake failure every sweep that
  reads like the *peer's* fault (#1136);
- a `client_cert_file` without its `client_key_file`, or the reverse — a
  half-configured mTLS target cannot complete a handshake, and refusing it here
  costs one startup rather than one alert per interval;
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

## Runtime target sets (#936)

Which targets this sensor polls is no longer a restart-only decision.

| | |
|---|---|
| `@desired/state/<host>/probe/targets` | a fleet's whole set for this host |
| `@rpc/probe/targets` | what it is polling **right now** |
| `@rpc/probe/targets/set` | replace it, without a restart |
| `state/probe/applied/targets` | which writer went last (`file` / `desired` / `rpc`), and the last refusal |

A `Delete` on the desired key reverts this host to the target list in its own
config file — never to an empty set.

`targets/set` is `fanout = "forbidden"`, unlike `thresholds/set`. A threshold is
the same rule wherever it lands; a target set is not. Pushing one fleet-wide
would tell every host to poll the same things from every vantage. Fleet-wide
target changes go through `@desired`, which is per-host by construction.

### What the wire cannot carry

`ProbeTargets` is the file-config target **minus `headers`**. That omission is
the point of the type: a header is where an `Authorization: Bearer …` lives, and
unlike SNMP's credentials it does not go through the secret resolver — what is
in the file is the literal token. A payload that carries one is **refused**, not
silently stripped, so an operator learns where headers belong instead of
watching a probe run without them.

Headers for a fleet-authored target come from **this host's** file-config target
of the same name. A fleet says *which* endpoint to check and how; the host says
what authenticates it.

A set is refused **whole** if any name is empty, duplicated, or not a legal key
chunk. A target's name becomes a key chunk and part of the alert key, so two
targets sharing one collapse onto a single series and a single alert that flap
over each other — and nothing anywhere reports it.
