# probe — the seven assertions

Every rule reconciles on **every sweep**, against the *last known* result for
every target — not only the ones checked this tick. Without that, a target on a
slow interval would have its alerts resolved and re-fired every time a faster
one ran.

Every alert carries `probe`, `target`, `kind` and — the one that makes them
actionable — **`vantage`**: where the check looked from. Two hosts reporting
different results for the same URL is not a contradiction to resolve; it is the
finding.

Not `duration_ms`. Labels are an alert's identity (`alert_key` hashes them), and
a per-check wall-clock in the labels re-keyed every alert every sweep, so none
ever stayed on one key long enough for `for_secs` to elapse — a target that was
down for a week paged nobody. The measurement is a telemetry point
(`{target}/duration_ms`) and, for a timeout, part of the summary sentence.

## Reachability

| Rule | Fires when | Severity |
|---|---|---|
| `probe-timeout` | the check hung for its full deadline | critical |
| `probe-down` | the check failed for any other reason | critical |

**A timeout suppresses `probe-down`.** One page arrives, with the right
diagnosis, instead of two with a vaguer one on top. The distinction is the
whole point: a hang means packets are going somewhere that never answers; a
refusal means the service said no. On 2026-08-20 that difference was the answer,
and finding it by hand took eight days — via a CI job's *duration*, which is
why the duration is a label and a gauge rather than a detail in a log line.

## HTTP

| Rule | Fires when | Severity |
|---|---|---|
| `probe-redirect-off-host` | **any hop** of the redirect chain left the configured host and `allow_offhost_redirect` is false | warning |

A probe that silently follows a redirect somewhere else is checking something
other than what it was asked about, and reporting that as success is how an
outside-in check becomes decorative. Off-host redirects therefore also fail the
check itself.

### Following is per target, and done by hand (#1134)

reqwest's redirect policy lives on the **client**; `follow_redirects` lives on
the **target**. A shared client that followed could not honour a target that
said not to, and for a release it did not — the field was documented,
wire-carried and read nowhere. So the client is built with
`Policy::none()` and `check::http` walks the chain itself. Three things follow:

- `follow_redirects: false` means the 3xx **is** the answer, which is the
  whole point of the flag: `{follow_redirects: false, expect_status: [200]}`
  against an endpoint that starts answering `302 → /login` now reports down,
  where before it followed, got 200 from the login page and reported up;
- `redirects` is the **chain**, in order, which is what the README has always
  promised. It used to be "the final URL if it differs from the configured
  string" — so `https://example.com` reported a redirect to
  `https://example.com/` every poll, which is a URL parser normalising, not a
  server redirecting;
- **every hop** is host-checked, and the comparison is lowercase and
  bracket-safe. A target written `Example.com` used to report an off-host
  redirect against its own answer, and `https://[::1]:8443/` reported one
  forever, because the port split cut the address itself.

A 3xx with no `Location` is not a redirect to anywhere: it is the response, and
`expect_status` judges it. A `303` becomes a `GET`, as the spec requires.

Unexpected statuses and missing body text fail the check and surface through
`probe-down`, with the status named in the error rather than a generic
"failed".

## TLS — from a socket or from a file

| Rule | Fires when | Severity |
|---|---|---|
| `probe-certificate-expiring` | `days_to_expiry <= expiry_warn_days` (30) | warning, **critical** at `<= expiry_critical_days` (7) |
| `probe-certificate-chain-invalid` | the presented chain did not validate | critical |
| `probe-certificate-name-mismatch` | the certificate does not cover the name asked for | critical |

`days_to_expiry` goes **negative** once expired, and the summary says
"EXPIRED N days ago". Clamping at zero would make "expired an hour ago" and
"expires in a month" look equally survivable on a graph.

`chain_valid` and `san_matched` are `Option`, and **`None` never fires**. A PEM
on disk has no chain to validate against a trust store and no name was asked
about; inventing either verdict would be worse than publishing none.

`inspect_untrusted` (default on) completes the handshake even when validation
fails, so an expired or self-signed certificate is *reported* rather than
collapsing into a bare connection error. Reading a certificate is not trusting
it: the real verifier's verdict is what gets published, and the bytes are only
ever parsed.

## DNS

| Rule | Fires when | Severity |
|---|---|---|
| `probe-dns-unexpected-answer` | the answer contained **none** of the addresses in `expect_addrs` | critical |

The alert names **which resolver answered**. That is the check: the 2026-08-20
hairpin was one host's resolver giving a different answer from every other
host's, and without the resolver in the record the sentence "resolves to the
wrong address here" cannot be written down.

## What is deliberately not asserted

- **Latency thresholds.** `duration_ms` and `http_ttfb_ms` are published and an
  operator can alert on them downstream. A built-in "slow" rule would need a
  number this sensor cannot know.
- **Content beyond a literal substring.** A body matcher that grows a query
  language becomes a second, worse test framework.
- **That the service is reachable from the internet.** A probe running on the
  server cannot tell you the server is unreachable. Nothing here claims
  otherwise, and the sensor logs that sentence at startup.
