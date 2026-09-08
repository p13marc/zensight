# The audit trail — what a write procedure records, and what it cannot say

**SYS-SUP-019** asks the platform to *journal every user action*. Issue #957 is
that requirement; this page is what it delivers, and — at least as important —
what it deliberately does not claim.

Before it, the only trail in the tree was the `systemd` sensor's bounded
in-memory ring on `@rpc/systemd/actions`: per-sensor, volatile, lost on restart,
and covering **one** write surface out of twelve. `grep -rn 'zensight::audit'`
matched exactly one file.

## The shape of it

Every procedure the registry declares `kind = "write"` records **both**
outcomes — executed *and* refused — through
[`zensight_common::audit::record`](../src/audit.rs). A refusal is at least as
interesting as an execution: it is the evidence that a gate did its job, and
the answer to "what is switched off on this host".

**Reads are not recorded.** A trail that also carries every `introspect` is a
trail nobody reads.

### It is a type, not a convention

There is no rule saying "remember to call `audit::record` on both arms".
[`served::WriteQuery`](../src/served.rs) exposes **no** `reply` and **no**
`reply_err`; the only two ways to answer are `executed` / `executed_but` and
`refused`, and each writes the record before it replies. An unaudited answer to
a write procedure is not something a call site can spell.

The record goes out *before* the reply on purpose: a lost reply is a retry, a
lost record is a hole.

Three checks back that up, in decreasing order of strength:

1. **The type**, above.
2. **`serve_queryable` debug-asserts** when the key it is handed is a declared
   write — debug builds die at startup, release builds warn. The classification
   is not a guess: it reads the registry's own `kind = "write"` column.
3. **`served::check_write_coverage`**, run by every producer at the moment it
   starts serving `introspect` — after its procedures are declared, before
   `alive` says it is callable. It reports every declared write this build
   serves through the plain seam. This checks a *declaration*, which is an
   observable event; nothing can watch a hook fire at runtime, and this page
   does not pretend otherwise.

   "Every producer" is now true (#1087). It has two spellings, because the key
   a procedure is served on depends on who is serving it:
   `check_write_coverage(producer)` derives the key from
   `PROFILE.local_origin()` and is right for a **sensor**;
   `check_write_coverage_keys(producer, keys)` matches on the procedure path
   and is right for a **service origin**, where it rides along inside
   `await_served`. Before that split the sensor-shaped check was the only one:
   pointed at the catalog it matched nothing, reported nothing, and read
   exactly like a clean bill of health — so the producer with the most write
   procedures in the tree was the one nothing checked. (They *were* declared
   through the audited seam. Nothing was verifying it.)

   And a producer whose slice is **missing or unparsable** is a finding, not a
   pass. Both checks used to return an empty result in that case, so a typo'd
   producer name turned the honesty check into a silent success.

A CI grep guard sits beside the four in `.forgejo/workflows/ci.yml`'s `lint`
job. It is a tripwire for the branch that never runs a sensor's tests, not the
enforcement.

## The record

One line of `key=value`, in a fixed order, absent fields omitted:

```
op=zensight-write procedure=systemd/action/set producer=systemd origin=h-4f2a
  target=nginx.service verdict=refused refused_by=actions.enabled
  caller_zid=a1b2c3 actor=alice request_id=r-77 error=... res=0 ts=1756000000
```

| field | meaning |
|---|---|
| `procedure` | the registry key, `<producer>/<path>` |
| `producer`, `origin` | who served it, and under which origin |
| `target` | what was acted on — unit, outlet, topic, entity pair |
| `verdict` | `executed` (the gate said yes and we acted) or `refused` |
| `refused_by` | **the switch that refused**, as a field (#866) — not prose to grep |
| `caller_zid` | the querier's Zenoh session id, when it published one |
| `actor`, `request_id` | `?actor=` / `?request_id=` from the selector |
| `error` | why an *executed* action did not achieve its aim |
| `res` | `1` only when permitted **and** achieved — what `ausearch --success` reads |
| `ts` | our clock, for the log fallback; the kernel stamps its own |

**Absent is absent.** An empty `?actor=` is dropped rather than emitted as
`actor=`, because "the caller claimed to be nobody" and "the caller said
nothing" are different statements and the person reading an audit trail is
exactly the person who cares which.

Values that are not plain tokens are **hex-encoded**, auditd's own convention
for untrusted strings: a unit name containing `verdict=executed` must not be
able to forge a second field.

**The size rule, exactly** (#1086). Every value is capped at **256 bytes** — a
byte budget, cut on a character boundary, with a `...` marker — and the whole
line is capped at **8 000 bytes**, under the kernel's
`MAX_AUDIT_MESSAGE_LENGTH` (8970), where a datagram is dropped without a word.
Both caps are applied where the line is *rendered*, so no caller can bypass
them: `target` and `error` are plain public fields that callers assign
directly, and `artifact/request` takes its `target` verbatim out of caller
JSON. A line that had to drop a field says `truncated=1`; `res` and `ts` are
never dropped, because those are what a reader selects on.

The three things that were wrong before, each of which produced the silent
audit path this module exists to prevent:

- the cap was *checked* in bytes and *applied* in characters, so 256 four-byte
  characters passed at 1 024 bytes — 2 048 once hex-encoded;
- `target` and `error` were never capped at all, so a bus caller sending
  `{"kind": "<20 KB>"}` produced a ~40 KB line;
- the kernel's ack was read for the **first record only**, so a refusal was
  never noticed, later acks accumulated unread in the socket buffer, and
  `emit` reported success for records that were dropped. Every record carries
  `NLM_F_ACK`, so every one is now read back and **matched by sequence
  number** — an ack attributed to the wrong record is worse than none.

## What this does NOT say

It records *what was requested and what happened on this host*. It does **not**
say **who asked**. The bus caller is anonymous:

- `caller_zid` identifies a **session, not a person** — and today it is
  essentially always absent, because nothing in ZenSight populates a query's
  source info and zenoh's constructor for one sits behind an `internal` feature.
  The field is read anyway: the day a peer or an upstream fills it in, it costs
  nothing to already be recording it.
- `actor` and `request_id` are **unauthenticated claims**. Whatever the caller
  typed.

Real attribution needs a caller *identity* — Zenoh mTLS (certificate CN) plus
Zenoh's ACL. That is a scope question named in epic #952 and deliberately not
answered here: #903 dropped RBAC, tenancy and audit-as-a-subsystem from the 1.0
roadmap on purpose, and this issue does not reopen it.

**Do not use these fields as an authorization input.** They are a correlation
hint.

## Delivery, and how it degrades

With the **`linux-audit`** feature: one `AUDIT_USYS_CONFIG` (1111) netlink
datagram per record, straight to `NETLINK_AUDIT`. Findable with:

```bash
ausearch -m USYS_CONFIG -i | grep zensight-write
```

The feature is **off by default**, the same rule as `icmp` / `ebpf` / `nvml`:
writing to the audit socket needs `CAP_AUDIT_WRITE`, which a container
deployment does not have and must not be made to want. Turn it on per binary:

```bash
cargo build --release -p zensight-sensor-systemd --features zensight-common/linux-audit
```

and give the unit the capability:

```ini
AmbientCapabilities=CAP_AUDIT_WRITE
```

It is **not** the C `libaudit`, despite what #957 first called the feature.
That library is LGPL-2.1+ and `deny.toml` grants LGPL only as a named per-crate
exception; the record we send is one header and one line of text, so we write
the netlink datagram ourselves over MIT `netlink-sys`, which was already in the
lock file. No licence exception, no `libaudit-dev` on a build host. The feature
is named after the subsystem it writes to.

Three delivery states, and the sensor's startup line says which one it is in
(`audit_delivering=`):

| state | what happens |
|---|---|
| feature off | every record goes to `tracing`, target `zensight::audit`, same fields |
| feature on, no socket or no capability | the **first** record's netlink ack is read; a refusal is logged once at `error`, naming `CAP_AUDIT_WRITE`, and the process falls back to `tracing` for its lifetime |
| feature on, delivering | one datagram per record |

The ack matters. A netlink permission failure comes back as an `NLMSGERR`
datagram, **not** as a `sendmsg` error — so a writer that only checks the send
return reports every record as delivered while the trail silently does not
exist. That is the failure mode this module exists to avoid: *a silent audit
path is worse than none, because it looks like one.*

The send itself is `MSG_DONTWAIT`. The `@rpc` handler loop is serial by design
(see `zensight_sensor_core::rpc`), and auditd backlog pressure can otherwise
park a blocking send for `audit_backlog_wait_time` — a minute by default — with
every queued call behind it.

## The loop closes with no new transport

`auditd` records land in journald. The `logs` sensor already ingests journald
and already security-tags anything carrying `_AUDIT_TYPE_NAME` (#107), so a
record comes back onto the bus as an event with
`sd.journald.audit_type=USYS_CONFIG` and `sd.journald.category=security`, and
into the GUI Logs view. Nothing in `zensight-sensor-logs` had to change.

## The one exemption

`audit::NOT_AUDITED` is a named list, and it has one entry:

| producer | procedure | why |
|---|---|---|
| `parallax` | `stream/report` | receiver feedback, not a command. RFC 07 §1.2 forbids it from changing anything, and it arrives once per interval per (consumer, stream, tier) — auditing it would bury every real action under records of a measurement. |

A test asserts every entry still names a currently-declared write procedure, so
a rename fails the exemption instead of quietly excusing nothing.

## Covered surfaces

Everything the registry declares `kind = "write"` — 35 declarations across 12
slices — reaches the trail, by one of two routes:

- **Automatically**, for anything served through `zensight_sensor_core::rpc::serve`
  or `serve_topic`: hostspec and systemd `expectations/set`, netlink
  `expectations/set` and `collection/set`, logs `rules/set`.
- **Through the audited seam at the call site**, for the hand-rolled `select!`
  multiplexers: systemd `action/set`, netring's four `*/set` topics, parallax
  `stream/set`, logs `filter/set`, the framework's `artifact/request` and
  `artifact/cancel` (which covers every artifact-capable sensor at once), and
  the correlator's catalog `link` / `unlink`.

A **gated** write records too. `served::serve_unavailable` — the seam a producer
uses to answer `error/gated` or `error/unsupported` for a surface it advertises
but cannot currently serve (#648) — routes write keys through the audited path,
because somebody trying to change something while the switch is off is the most
interesting refusal there is.
