# container — the seven assertions

Every rule reconciles on **every sweep**, so a condition that clears resolves —
including one whose container disappeared. All alerts carry
`AlertKind::Expectation`, land on `state/container/alert/{alert_key}`, and
carry `container`, `image` and — when the container is Quadlet-managed —
`unit`, so an operator paged by one gets something restartable rather than a
container id.

The device slug is the container **name**, not its id: an id changes every time
the container is recreated, which is every deploy, and a series that forks on
every deploy is a series nobody can read.

## Health

| Rule | Fires when | Severity |
|---|---|---|
| `container-unhealthy` | a configured healthcheck is failing | warning |
| `container-healthcheck-never-ran` | a healthcheck is configured and has **never produced a result** | warning |

The second is the garage case and it is deliberately a separate rule with a
separate sentence. A `CMD-SHELL` probe in a distroless image cannot execute;
the runtime reports `unhealthy` forever and the service is fine. garage was in
that state from the day it was deployed and nobody noticed for weeks, because
"the probe is broken" and "the service is failing" rendered identically —
and the summary text sends an operator to two completely different places.

Consequently the `{name}/healthy` gauge is **not published** for a check that
has never run, nor for a container with no healthcheck. A `0` in either case
tells every dashboard the service is down.

## Lifecycle

| Rule | Fires when | Severity |
|---|---|---|
| `container-restart-loop` | more than `restart_max` restarts within `restart_window_secs` | critical |
| `container-oom-killed` | the cgroup's `oom_kill` counter advanced; held for `oom_hold_secs` (default 600) | critical |
| `container-exited-nonzero` | the container is not running and its last exit code is non-zero | critical |

**The first two are delta rules.** `restart_count` and `oom_kill` are
cumulative; firing on the total would alert forever about a kill from last
year. The previous sweep's values are the baseline, which has two consequences
worth stating:

- nothing delta-shaped fires on the first sweep after a sensor restart, and
- a restart-loop alert only fires while the baseline is younger than the
  configured window — the same total spread over a day is not a loop;
- the OOM half of the baseline is **held still for `oom_hold_secs`** once a
  burst of new kills begins. A kill is a one-sweep event, and the alert has a
  `for_secs` debounce that must see the condition on more than one sweep; with
  the shipped 30 s poll and 60 s debounce the condition used to be true for
  exactly one sweep, and the rule could never fire. The alert now stays up for
  the hold window and resolves on its own afterwards.

Labels are an alert's identity (`alert_key` hashes them), so **no rule puts a
measurement in a label**: the failing-check streak and the kill count ride in
the summary sentence. A label that changed every sweep re-keyed the alert every
sweep, which is another way to never fire.

The OOM alert names the container **and its `memory.max`**. On 2026-08-17 five
sensors shared one cgroup and one `MemoryMax`, so per-container memory did not
exist as a number and the kill was attributed to "the bundle" for eleven days.
This rule is that number, and the alert text is the sentence nobody could write
at the time.

## Supply chain

| Rule | Fires when | Severity |
|---|---|---|
| `container-image-behind-upstream` | the running digest differs from what the configured tag resolves to upstream | info |
| `container-image-unsigned` | the registry holds no cosign signature object for the running digest | warning |

Both need `container.upstream.enabled` — the one collector that leaves the
host, off by default, and restricted to a named registry allowlist.

**Neither fires on "not checked".** An image whose upstream was never resolved
is not behind; an image whose signature was never looked for is not unsigned.
Without that rule every deployment with the collector off would look like a
supply-chain failure, and the alert would mean nothing.

`container-image-unsigned` proves a signature **exists**, not that it verifies:
verification needs the public key and belongs in a tool that has one. Presence
is the strongest honest claim, and it is exactly the claim that would have
caught cosign signing nothing for eight days.

## What is deliberately not asserted

- **Anything requiring a write.** The socket client has two methods and both
  are GETs. Restarting a container is a different threat model.
- **Whether the service inside works.** The healthcheck is the runtime's
  opinion; a probe from outside is what `probe` (#820) is for.
- **Memory thresholds.** `{name}/memory_ratio` is published and an operator can
  alert on it downstream, but a built-in "container near its limit" rule would
  fire constantly on every correctly-sized container: sitting near a limit is
  what a limit is for. The OOM counter is the fact that means something.
