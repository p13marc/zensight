# zensight-sensor-probe

The outside-in view (#820).

Everything else ZenSight measures is *inside*. Nothing checked that the thing
works from **outside** — that the site answers, that the certificate is valid,
that the name resolves, that the port is reachable from where a user actually
is.

## What that cost, twice

**The `/etc/hosts` hairpin, 2026-08-20 → 2026-08-28.** A reboot dropped a hosts
entry, so a guest resolved `git.marcpardo.eu` to the public IP — which a guest
cannot reach, because the edge DNAT matches the external interface only. cosign
and Renovate both broke. **The diagnosis took eight days.** It eventually hinged
on someone noticing that failing CI runs took *2m16s* — a 20 s connect timeout
repeated — and that the forge's router log showed zero requests. A guest-side
probe of that URL would have said *"timeout, 20 s"* within one interval, on day
one. Instead the failures were first attributed to expired tokens, and two
issues were filed on that theory.

**TLS.** The only outside-in check the fleet had was a shell script probing
seven vhosts by SNI on a timer. External outage monitoring was still "pending
user setup" a year on.

## Three things this sensor is careful about

**A timeout is its own outcome.** Not a failure with different text. A hang
means packets are going somewhere that never answers; a refusal means the
service said no. Those are different diagnoses, `probe-timeout` is a separate
rule that *suppresses* the generic `probe-down`, and the **duration rides on
the alert** — because on 2026-08-20 the duration *was* the diagnosis, and
recognising it by hand took eight days.

**The vantage point is half the answer.** The same target checked from the
edge, from a guest and from a workstation gives three different, equally true
results. Two hosts disagreeing is not a contradiction — it is precisely the
shape of the hairpin. Every series and every alert is therefore filed under the
vantage point, never the target (#883) — two hosts checking the same URL are two
answers, not one collision. `vantage` rides on every result document and every alert,
so deploy this in more than one place *on purpose*.

**An absent verdict is not a negative one.** A PEM on disk has no chain to
validate against a trust store, so `chain_valid` is `None` and no gauge claims
otherwise. A check that did not run — an `icmp` target in a build without the
feature — says so explicitly, and the sentence says it is *not evidence about
the target*.

## The one non-network check

**Local certificate files.** Read a PEM off disk and publish its `notAfter`,
through the same parser the socket path uses, so a handshake and a file produce
identical documents. It retires the monthly cron that warns when a ZenSight
*mesh* certificate is within 60 days of expiry — and removes the oddity of a
supervision system needing an external timer to watch its own certificates.

## What it does not do

**A probe running on the server cannot tell you the server is unreachable.**
This does not replace external outage monitoring, and a deployment that treats
it as a replacement has a blind spot exactly where it believes it has coverage.

It is also a **client only**: it opens the connections its config names and
nothing else. No listeners, no write surface, and the registry slice declares
no `write` procedure — with a test that fails if one appears.

## Bounded by construction

An explicit target list, a per-target interval floored at 5 s so this cannot be
configured into a load generator, a concurrency cap across all targets, and a
timeout that **startup refuses** unless it is shorter than the target's own
interval. A check that can outlive its own tick is queued, not bounded.

## Checks

| Kind | Publishes |
|---|---|
| `http` | status, expected-status match, optional body match, TTFB, total, redirect chain — and **fails on a redirect that leaves the configured host** unless told otherwise |
| `tls` | chain validity, **days to expiry** (negative once expired), issuer, subject, SANs, SAN match, protocol |
| `dns` | answers, resolution time, and **the resolver, named** — without which "resolves to the wrong address" is not expressible |
| `tcp` | connect success, connect time |
| `icmp` | reachability — build feature `icmp`, off by default, needs `CAP_NET_RAW` |
| `certfile` | a PEM's `notAfter`, with no network at all |
| `ntp` | **clock offset, delay, stratum, leap and reference id** from an SNTP query (RFC 4330) — one UDP exchange, no privilege, and it never sets the clock |
| `burst` | **latency, jitter and loss for a link** — `count` probes `spacing_ms` apart in one interval, reduced to rtt min/avg/max/p95, mean absolute IPDV and loss % |

### What the `ntp` check does and does not tell you

**`offset_ms` is measured against the probe host's own clock**, which is the
only clock this process has. It is a statement about the *relationship* between
two clocks, not about either being right: a probe host that is itself an hour
out reports every server as an hour out. Pair it with
`state/sysinfo/timesync` — sysinfo's opt-in `collect.timesync`, which reports
the **local** discipline — to tell the two cases apart.

The check fails on the **server's own statement** that it is unusable: leap
indicator 3 (unsynchronised) or stratum 0 (a kiss-o'-death refusal, whose code
— `DENY`, `RATE` — is published verbatim, because those are the two answers an
operator most needs to see and both are otherwise indistinguishable from a
silent failure).

It exists because "is `chronyd` active" was the only NTP coverage in the tree,
and that is true of a `chronyd` that has never reached a server: the daemon
runs, the unit is green, and the clock is wrong.

There is **no offset threshold** — a number this sensor cannot know, the same
stance it takes on latency. `clock-offset-high` arrives with #931's shared
`ThresholdsConfig`.

### What a burst is careful about

A single-shot check per interval cannot produce a jitter figure at all — one
sample has no variation. Three things follow, and each is the difference
between a number and a wrong number:

- **A total loss publishes `loss_pct: 100` and no RTT series at all** — not
  zeros. A zero is indistinguishable from a perfect link, and a dashboard
  averaging it improves the fleet's numbers every time a link dies.
- **Jitter spans only *consecutive* successes.** Bridging a gap would report
  the gap the loss left as delay variation. A burst with fewer than two
  consecutive successes publishes loss and RTTs but no jitter.
- **Startup refuses a burst that cannot finish inside its own interval.**
  Overlapping bursts do not merely queue: the figures then describe two
  overlapping bursts rather than one link.

`tcp` transport works in a default build with no capability; `icmp` needs the
`icmp` feature and `CAP_NET_RAW`, and startup refuses it in a build without
them. The two are **not comparable** — a TCP connect RTT includes the peer's
accept path — so the transport is published beside the numbers.

There is **no built-in jitter or loss threshold**, for the reason this sensor
refuses built-in latency thresholds: a number it cannot know. The figures go on
the bus for the GUI, the exporters and the historian; thresholds arrive with
the shared `ThresholdsConfig` (#931).

## Running it

```bash
just probe      # after filling in configs/probe.json5
```

Every example target in the shipped config is commented out, because no
generator can invent a URL worth watching — and a probe pointed somewhere by
default would be a monitoring tool making requests nobody asked for. It is
therefore not part of `just run`. It *is* in the CI conformance roster: with an
empty target list it reaches nothing at all, declares its slice, serves it, and
is judged like every other producer.

## Docs

| | |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | every key, and why each default is what it is |
| [`docs/assertions.md`](docs/assertions.md) | the seven rules |
| [`src/lib.rs`](src/lib.rs) | scope and non-goals |
