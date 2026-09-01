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

## Validation

Every problem is reported at once:

```
container.timeout_secs (10) must be shorter than container.poll_interval_secs (5) …;
container.upstream.enabled is set but container.upstream.registries is empty …
```

`shipped_config_parses` (in `src/config.rs`) loads `configs/container.json5`
and validates it, so the example cannot rot unnoticed.
