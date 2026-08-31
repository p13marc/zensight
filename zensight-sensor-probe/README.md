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
shape of the hairpin. `vantage` rides on every result document and every alert,
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
