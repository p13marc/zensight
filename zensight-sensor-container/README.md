# zensight-sensor-container

The whole workload, previously invisible (#819).

Every service on the reference fleet is a Podman Quadlet container, and **no
sensor knew what a container was**:

- sysinfo's `cgroups` collector can be pointed at explicit `cgroup_paths`, is
  off by default, and does not enumerate — so it cannot discover containers.
- The systemd sensor sees `caddy.service` as a unit. It does not know that unit
  is Caddy 2.11.4, or which image digest it runs, or that its healthcheck has
  been failing since the day it was deployed.
- netlink surfaces the podman bridges' containers as *observed entities* —
  eleven rows of a catalog with IPs and nothing else.

Four separate findings of the 2026-08-28 audit are fields this sensor
publishes:

| Found by hand | Published here |
|---|---|
| **garage reported `unhealthy` from the day it was deployed** while serving traffic perfectly — a distroless image with no `/bin/sh`, so a `CMD-SHELL` check could never pass. Nobody noticed for weeks. | `HealthState::NeverRan` — a rule and a sentence of its own |
| **cosign silently signed nothing for eight days** | signature presence |
| **12 pinned images behind upstream**, surfaced by a monthly mail | a live digest comparison |
| the 2026-08-17 memory incident attributed to *"the bundle"* for eleven days | per-container `memory.current`, `memory.max`, `oom_kill` |

## Two sources, joined

**The runtime socket** knows what a container *is*: image reference and digest,
healthcheck state, restart count, last exit code, ports, mounts, restart
policy, and — through the `PODMAN_SYSTEMD_UNIT` label — **the systemd unit that
owns it**, which is what makes a container join up with the systemd sensor's
view instead of sitting beside it. Every alert carries that unit, so an
operator paged by one gets something restartable rather than a container id.

**The kernel** knows what it is *doing*: cgroup-v2 `memory.current`,
`memory.max`, `memory.peak`, CPU time and throttling, `oom_kill`, and PSI.

## The distinction that matters most

`unhealthy` and *"the healthcheck has never produced a result"* are different
facts, and they had been rendering as the same one. A `CMD-SHELL` probe in an
image with no shell cannot execute; the container reports `unhealthy` forever
and the service is fine. Telling an operator their service is failing sends
them to debug the wrong thing — which is exactly what happened, for weeks. So
`container-healthcheck-never-ran` is a separate rule with a separate sentence,
and the `{name}/healthy` gauge is **not published at all** in that state: a `0`
there would tell every dashboard the service is down.

## Read-only, and no egress by default

- The socket client has **two methods and both are GETs**. There is no way to
  start, stop or change anything, the registry slice declares no `write`
  procedure, and a test fails if one appears. The shipped units mount the
  socket `:ro` anyway — a posture that depends on the code being right is not
  a posture.
- Exactly one collector leaves the host: the upstream-digest and signature
  checks. It is **off by default**, and when on it is restricted to a named
  registry allowlist — startup *refuses* `enabled` with an empty list, because
  an allowlist that defaults to everything is not one. Requests are anonymous
  and read-only; no credentials are read, sent, or stored.
- "Not checked" is never reported as "unsigned" or "behind". Silence is not
  evidence, and treating it as evidence would make every deployment that leaves
  the collector off look like a supply-chain failure.

## Counters are graded on the delta

`restart_count` and `oom_kill` are cumulative. Firing on the total would alert
forever about a kill from last year, so the previous cycle's values are kept as
a baseline and the rules read the difference — which also means **nothing
delta-shaped fires on the first sweep after a restart**.

## Running it

```bash
just container                        # against the local runtime socket
```

Sockets are found automatically, in order: rootful podman
(`/run/podman/podman.sock`), this user's rootless podman, then
`/run/docker.sock`; only the ones that exist are polled. It is deliberately
**not** part of `just run`: on a host with no runtime it would report a failure
every cycle for something that host simply does not do.

`packaging/systemd/` and `packaging/quadlet/` ship units. The quadlet mounts the
socket and the cgroup tree read-only.

## Docs

| | |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | every key, and why each default is what it is |
| [`docs/assertions.md`](docs/assertions.md) | the seven rules and what each catches |
| [`src/lib.rs`](src/lib.rs) | scope and non-goals |
