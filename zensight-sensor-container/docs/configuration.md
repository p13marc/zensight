# container — configuration

`configs/container.json5` runs unedited on a host with podman: the sockets are
found automatically and every assertion is on. What an operator normally
touches is the egress block, and only if they want it.

## Keys

| Key | Default | Note |
|---|---|---|
| `container.sockets` | `[]` | empty = the conventional paths, in order; only the ones that exist are polled |
| `container.source` | the hostname | the reporting host every series and alert is filed under. A container name is unique *per host*, not globally, so the container rides in the key path and in the `container`/`image`/`unit` labels — never as the identity of the series (#883/#884) |
| `container.poll_interval_secs` | `30` | |
| `container.timeout_secs` | `10` | **must be shorter than the interval** |
| `container.cgroup_root` | `/sys/fs/cgroup` | override when the sensor itself runs in a container against a bind-mounted host cgroupfs |
| `container.ignore` | `[]` | container names to skip entirely |
| `container.evidence` | `true` | identity claims (name + IPs) so netlink's wire-only bridge entities merge in |
| `container.upstream.*` | **off** | the one collector that leaves the host — see below |
| `container.alerts.*` | see [`assertions.md`](assertions.md) | |

### Socket discovery

Tried in order, and only if present:

1. `/run/podman/podman.sock` — rootful podman
2. `$XDG_RUNTIME_DIR/podman/podman.sock` — this user's rootless session
3. `/run/docker.sock` — Docker, via the compatibility API

Rootful first: a sensor running as root on a host with both sees the system
containers, which is what a fleet cares about. Several sockets can be polled at
once; **some** unreachable is normal (a host with rootful podman and no
rootless session is the common case) and is logged at debug. **All** unreachable
is a real failure and is reported as one — "no runtime" and "no containers" are
different answers, and rendering both as an empty list would make a host whose
podman is dead look like a quiet host.

The socket is mounted `:ro` by the shipped units. The client has two methods
and both are GETs, but a security posture that depends on the code being right
is not a posture.

### Rootless containers

A rootless session's containers live under `user.slice`, and a sensor in a
different session cannot read their cgroups at all. The runtime reports the
path and the sensor follows it; when the files are unreadable every resource
field is `None` — never zero, which would read as "idle".

### What counts as a device

`devices_total` is the number of containers this sweep found, and
`devices_responding` counts the same population (#1088). Two things used to
make them disagree, permanently and in the wrong direction:

- The runtime **socket** was recorded as a device success alongside every
  container, so `devices_responding` was one higher than `devices_total` even
  on a host that never redeploys. The socket answering is the *sensor* doing
  its job, not a device, and it is recorded as such now.
- `device_liveness` had **no eviction**. Containers are recreated with new names
  on every deploy, so the map accumulated every name it had ever seen and grew
  for the life of the process. The poller now retires the names that left the
  listing.

### The egress block

```json5
upstream: {
  enabled: false,
  interval_secs: 21600,     // 6 h; registries rate-limit and the answer
                            // changes on a release cadence, not a poll one
  signatures: false,        // needs `enabled`
  registries: [],           // REQUIRED when enabled
}
```

This is the only part of the sensor that contacts anything off the host. It
answers two questions a local socket cannot — is the pinned digest still what
the tag resolves to, and does a signature exist for it — and it replaces
`image-update-report.sh` and its monthly mail.

Startup **refuses** `enabled: true` with an empty `registries` list. An
allowlist that defaults to everything is not an allowlist, and a monitoring
agent should not decide on its own which third-party hosts to reach. Requests
are anonymous and read-only: no credentials are read, sent, or stored, and a
private registry answering 401 yields "not checked", which is honest, rather
than "unsigned", which would not be.

Turning it on logs a `warn` naming the registries, on every start.

## One-shot diagnosis

```bash
zensight-sensor-container --config /etc/zensight/container.json5 --diagnose
```

Every rule in [`assertions.md`](assertions.md) reads a field that can be
*silent* — a healthcheck the runtime never ran, a cgroup the sensor cannot
read, an `oom_kill` counter that is absent rather than zero, a signature that
was never looked for — and the sensor reports each silence as a silence. An
operator still has to find out **which** silence they have. `--diagnose`
answers that in plain sentences and exits: which conventional sockets are
present and which absent (absent is normal for a runtime the host does not
run), what each socket lists, and per container every input of the seven
rules with the verdict it produces — `never ran` versus `unhealthy`, an exit
code the runtime did or did not report, the running digest (which the Docker
compatibility API omits), the upstream digest and the signature **with the
reason** when the registry did not answer, and the cgroup directory with each
file read or named unreadable. It is read-only and **never opens a Zenoh
session**: debugging a socket permission should not join a fleet.

The egress block is honoured as configured — with `upstream.enabled: false`
nothing leaves the host and the diagnosis says so; with it on, the same
allowlist applies.

### Docker Hub needs a token even to read a public manifest

Found by the first `--diagnose` against a real socket (#947): Docker Hub and
ghcr.io answer an anonymous manifest `HEAD` with `401` and a
`WWW-Authenticate: Bearer` challenge, then hand a public repository's token
to anyone who asks the realm. quay.io answers anonymously. Before this the
sensor gave up at the `401`, so every `docker.io/library/*` image was "not
resolved" and `image-behind` could never fire on the reference fleet. The
sensor now follows the challenge — still anonymously: no credential is read,
sent or stored, and a `401` *after* the token (a private repository) stays a
non-answer, never "unsigned".

## Validation

Every problem is reported at once:

```
container.timeout_secs (10) must be shorter than container.poll_interval_secs (5) …;
container.upstream.enabled is set but container.upstream.registries is empty …
```

`shipped_config_parses` (in `src/config.rs`) loads `configs/container.json5`
and validates it, so the example cannot rot unnoticed.
