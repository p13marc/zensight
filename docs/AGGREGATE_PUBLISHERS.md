# Aggregate Publishers

`aggregate-publishers` is an **opt-in, additive Cargo feature** available on
three sensors. When enabled, a sensor publishes a single **typed, structured
object per host** alongside its normal per-metric `TelemetryPoint` stream, so a
consumer (for example a supervision HMI) can `get`/`subscribe` one object and
read it directly instead of re-assembling state from the metric firehose.

The feature is **off by default**. When it is off, the relevant code is not
compiled and each sensor builds and behaves exactly as before — no new keys, no
behavioural change.

```bash
# Enable per crate:
cargo build -p zensight-sensor-sysinfo --features aggregate-publishers
cargo build -p zensight-sensor-netlink --features aggregate-publishers
cargo build -p zensight-sensor-syslog  --features aggregate-publishers
```

No runtime toggle is needed: the aggregate is produced whenever the feature is
compiled in. All objects are serialized as JSON with `snake_case` fields.

## sysinfo — `HostInfo`

Published once per poll to `zensight/sysinfo/<host>/host`, reusing the state
already collected for the per-metric stream (no extra OS polling).

| Field           | Type      | Notes                                            |
|-----------------|-----------|--------------------------------------------------|
| `host`          | string    | Matches the `<host>` key segment                 |
| `cpu_cores`     | float[]   | Per-core usage, percent `0..=100`                |
| `mem_used_mb`   | uint      | Used physical memory (MiB)                       |
| `mem_total_mb`  | uint      | Total physical memory (MiB)                      |
| `disk_used_gb`  | float     | Used space across included volumes (GiB)         |
| `disk_total_gb` | float     | Total space across included volumes (GiB)        |
| `load_avg`      | float[3]  | `[1m, 5m, 15m]`                                  |
| `uptime_s`      | uint      | Uptime (seconds)                                 |
| `net_rx_bps`    | uint      | Aggregate receive throughput (bytes/s)           |
| `net_tx_bps`    | uint      | Aggregate transmit throughput (bytes/s)          |

Disk totals sum only the volumes the per-metric path already includes (the disk
filter is applied), so the aggregate matches the detailed metrics. Network
throughput is derived from the cumulative interface byte counters against the
previous tick (zero on the first poll).

## netlink — `HostInterfaces`

Published once per poll to `zensight/netlink/<host>/interfaces` with shape
`{ "host": "...", "interfaces": [NetIface, ...] }`.

### `NetIface`

| Field        | Type     | Notes                                                       |
|--------------|----------|-------------------------------------------------------------|
| `name`       | string   | Interface name (e.g. `eth0`)                                |
| `state`      | string   | `UP`, `DOWN`, or `DEGRADED` (admin-up but carrier down)     |
| `ip_address` | string?  | First IP bound to the interface, if any                     |
| `mtu`        | uint?    | MTU, if reported                                            |
| `rx_bps`     | uint     | Receive throughput (bytes/s), counter-derived               |
| `tx_bps`     | uint     | Transmit throughput (bytes/s), counter-derived              |
| `rx_errs`    | uint     | Cumulative receive errors                                   |
| `tx_errs`    | uint     | Cumulative transmit errors                                  |
| `bound_pids` | uint[]   | PIDs owning a socket bound to one of the interface's IPs    |

`bound_pids` is the **raw OS observable**: it is resolved from the sockdiag
socket inode plus a `/proc/<pid>/fd` scan (best-effort; sockets on wildcard
`0.0.0.0`/`::` listeners cannot be attributed to a single interface and are
skipped). Note that sockdiag itself exposes a socket **inode and uid, not a
PID** — the PID is recovered via the `/proc` scan.

> **Out of scope on purpose.** `NetIface` deliberately omits comm-domain
> concepts: there is **no** link `kind` (FO/RF/WIFI) and **no**
> `bound_driver_ids` (comm driver instance ids). Those belong to the comm domain
> (CommPlan, ACM), not to ZenSight's kernel observability. The consumer enriches
> the snapshot by correlating `bound_pids` / `name` with its own model.

## syslog — `LogEvent`

For every accepted message (after filtering), in **addition** to the existing
`TelemetryPoint`, the sensor publishes a typed event to
`zensight/syslog/<host>/events/<uid>`.

| Field       | Type    | Notes                                                       |
|-------------|---------|-------------------------------------------------------------|
| `uid`       | string  | Time-sortable: `<epoch_ms>-<counter>`, zero-padded          |
| `timestamp` | int     | Event time, epoch milliseconds                              |
| `severity`  | string  | `emerg`/`alert`/`crit`/`err`/`warning`/`notice`/`info`/`debug` |
| `facility`  | string  | Syslog facility keyword                                     |
| `app`       | string? | Application / process tag                                   |
| `pid`       | uint?   | Process id (only when the syslog tag is numeric)            |
| `message`   | string  | Message content                                             |
| `category`  | string? | Known systemd-event category (e.g. `coredump`), else null   |

The `uid` is constructed so that lexicographic ordering of keys matches
chronological ordering, and a monotonic counter keeps it unique within the same
millisecond. `category` reuses the same known-event catalog as the sensor's
alert path, so it stays consistent with raised alerts.

## Delivery semantics

These typed objects are published with a plain Zenoh `put` (the control-plane
path), not through the advanced-publisher telemetry cache. Live `subscribe`
delivery works as usual. If durable cold-start `get` (late-joiner history) is
required for these keys, back them with a Zenoh storage or an advanced publisher
in a follow-up — the current change keeps the write path minimal and additive.
