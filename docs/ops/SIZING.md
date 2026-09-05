# Sizing and retention

How much memory each component actually uses, how fast the two on-disk stores
grow, and what to do about both.

> **Status: the harness is in, the numbers are not.** Every table below is empty
> and marked *awaiting measurement*, because the only honest source for them is a
> real fleet over real time — #944 asks for fourteen days on the six-VM reference
> fleet, and that is elapsed time, not work. What has changed is that collecting
> them is now one command instead of a GET per host by hand. Fill the tables from
> a run; do not fill them from a laptop.
>
> Until then the shipped `MemoryMax` values remain **starting points**, and this
> page says which ones and what they were guessed from.

---

## 1. Measuring

```bash
# a quick look at what is running right now
scripts/fleet-sizing.sh

# the #944 soak: fourteen days, one sweep every twelve minutes
SAMPLES=1680 INTERVAL_SECS=720 HUB=tcp/<router>:7447 \
  scripts/fleet-sizing.sh

# re-read a finished run without re-running it
scripts/fleet-sizing-report.py target/fleet-sizing/<run>/
```

Run a long soak under something that outlives your shell —
`systemd-run --user --unit=fleet-sizing …`, tmux, or `nohup`. The run directory is
written sweep by sweep, so a run killed on day nine still reports nine days.

**What it reads.** Every sensor has published its own resource telemetry since
#811, in its health document's `self_stats`: RSS, virtual size, CPU, the declared
budget, per-table occupancy, the shed ladder's step, and — the field that makes
this a sizing document rather than a guess — the process's own cgroup-v2 reading,
`memory.max` and `oom_kills`. So the report compares observed peak against what
the host *actually allows*, not against what a unit file is supposed to say.

**What it refuses to do.** A producer that published no `self_stats` is reported
as *not measured*, never as zero. A producer that missed sweeps is marked
*partial*, because a percentile across samples it was dead for is a number about
nothing. And a run whose first sweep answers nothing fails immediately, naming
the endpoint — a fourteen-day soak that discovers on day fourteen that it dialled
the wrong router has cost fourteen days.

---

## 2. Per-sensor memory

*Awaiting measurement — run §1 and paste the report's table here, with the
window it was measured over.*

| host | producer | RSS p50 | RSS p95 | RSS max | CPU p95 | shipped `MemoryMax` | measured cap |
|---|---|---:|---:|---:|---:|---:|---:|
| | | | | | | | |

### What is shipped today, and where it came from

These are the values in `packaging/quadlet/*.container`. Every one of them was
chosen by reasoning about what a sensor does, not by watching one do it — which
is exactly why each unit file carries the line *"MemoryMax below is a STARTING
POINT (reference-fleet sizing); measure yours"*.

| Unit | Shipped `MemoryMax` | Why that number was guessed |
|---|---:|---|
| `zensight-sensor-netring` | 512M | the only sensor with unbounded-in-principle state: flow ring, TLS/asset tables, detector state |
| `zensight-historian` | 320M | holds a database; its own budget default is 256 MiB and this leaves the process room around it |
| `zensight-sensor-logs` | 256M | a durable store plus ingest buffers |
| `zensight-sensor-sysinfo` | 256M | generous for what it does; it is the sensor most likely to be the only one on a host |
| `zensight-sensor-netlink` | 128M | socket and neighbour tables scale with the host's connection count |
| `zensight-sensor-systemd` | 128M | one D-Bus connection and a unit watchlist |
| `zensight-sensor-bmc` | 96M | one HTTP client against one BMC |
| `zensight-sensor-pve` | 96M | one HTTP client against one API |
| `zensight-sensor-container` | 64M | a socket client with two GETs and cgroupfs reads |
| `zensight-sensor-hostspec` | 64M | a closed vocabulary of assertions; executes nothing |
| `zensight-sensor-probe` | 64M | a check client with a timeout |

**Replace a row only from a measured window**, and keep the shipped column beside
your own — the delta is the interesting part, and it is what a future default
should move toward.

### The fleet unit, and why it is one container per sensor

The all-in-one `zensight-sensors` bundle is a demo. A fleet runs one container
per sensor because the bundle's single cgroup is one `MemoryMax` pool that the
greediest sensor spends for everyone — and on 2026-08-17 that meant the OOM
victim's exit took down the four sensors that would have explained the incident.
See [`packaging/quadlet/README.md`](../../packaging/quadlet/README.md).

---

## 3. Historian disk growth and series count

*Awaiting measurement — the `@rpc/historian/stats` section of a soak's report.*

| | Observed | Target |
|---|---:|---:|
| Series | | ≤ 10 000 |
| Bytes per minute bucket, as written | | ≤ 48 |
| Bytes per minute bucket, after `compact()` | | ≤ 48 |
| Database size at defaults | | ≤ 2 GiB |
| Steady RSS at `budget_bytes = 256 MiB` | | ≤ 256 MiB |
| Worst-case prune | | ≤ 2 s |
| Range GET p95 (24 h, minute tier) | | ≤ 200 ms |

The targets, the bench that produced the laboratory figures, and what the bench
found on its first run are in
[`zensight-historian/docs/storage.md`](../../zensight-historian/docs/storage.md) —
**read it before filling this in**, because two of these numbers are already
known to miss at bench scale and the open question is whether a real fleet's
cardinality reproduces that. Tracking issue: #911.

Growth per day needs two stats readings separated by real time; one run reports
the file as it stands.

---

## 4. The events storage: retention is yours

The `events` class is append-only, and the router storage that persists it
(`configs/router-events-storage.json5`) is an `fs` volume.

> **Zenoh storages have no TTL.** With `fs`, retention is a disk-space concern:
> prune the volume directory on your own schedule, or size it against your event
> rate.

The number to size against is declared in the registry: SNMP traps at
`burst(1000/h)` with a cardinality bound of 100 000 per device. Multiply by your
device count and your retention in hours.

*Awaiting measurement.*

| | Observed |
|---|---:|
| Volume bytes after 14 days | |
| Growth per day | |
| Events per day | |

**A pruning recipe.** There is no in-Zenoh expiry, so this is a `find` on the
volume directory, run by a timer:

```bash
# keep 30 days of event files; adjust the path to your storage volume
find /var/lib/zenoh/events -type f -mtime +30 -delete
find /var/lib/zenoh/events -type d -empty -delete
```

Two cautions. Prune the **volume**, never the keyspace: RFC 04 §1.2 refuses a
wildcard delete as an operator act, and a `delete v1/*/events/**` cannot tell a
month-old record from one written a second ago. And if you want policy-based
expiry rather than a timer, the config file itself points at the alternative —
copy the shape of `configs/router-pdns-influxdb-storage.json5` and give the
storage a backend with a retention policy.

---

## 5. What the 2026-08-17 OOM looks like now

On 2026-08-17 the sensor bundle OOM-killed a 1 GB VM. The sensor went from
110 MB to 355 MB, reported `Healthy` the whole way, and the growth was found
eleven days later by hand. Nothing on the bus could have shown it, because at the
time nothing published a sensor's own memory.

That is no longer true, and the point of this section is that the next operator
should recognise the shape before the kill rather than after it. In order of how
early they appear:

1. **`self_stats.rss_bytes` climbing across sweeps with a flat workload.** The
   fleet-sizing report's `RSS p50` vs `RSS max` columns are this: a wide gap on a
   sensor whose input rate did not change is growth, not load.
2. **The `sensor-budget` alert.** A declared budget arms it: **Warning at ≥80%**
   of budget, **Critical at ≥95%**, releasing below 75%, and the message names
   the largest table — which turns "the sensor is big" into "the flow table is
   280 MB of it". A sensor with no declared budget arms no rule and its health
   document says so, rather than implying it is fine.
3. **`self_stats.ladder.step` above 0.** The shed ladder (#812) is enforcement:
   step 1 evicting, 2 degraded (optional work stopped), 3 saturated. **A sensor
   at step ≥1 is staying inside its budget by dropping work**, so its RSS is what
   the budget forced and not what the workload wanted — the fleet-sizing report
   refuses to size from such a row for exactly that reason. `ladder.futile` means
   eviction was tried and freed almost nothing: the memory is not in evictable
   tables and the only fix is a bigger budget.
4. **`self_stats.cgroup.oom_kills` non-zero.** Too late — this is the count of
   times the host already killed it. Every number measured on that process is
   from the survivors, so read its peak as a floor.

**Set a budget.** A sensor with no `budget_rss_mb` has no `sensor-budget` alert
and no ladder, so steps 2 and 3 above simply do not exist for it: it grows
silently until the cgroup kills it, which is precisely the 2026-08-17 sequence.
The budget is declared, the ladder enforces, and `MemoryMax` is the backstop —
set all three, and set the budget *below* `MemoryMax` so the ladder gets to act
first.

---

## See also

- [`packaging/quadlet/README.md`](../../packaging/quadlet/README.md) — the
  per-sensor units and why the bundle is a demo
- [`zensight-historian/docs/storage.md`](../../zensight-historian/docs/storage.md)
  — tiers, retention defaults, the bench and the acceptance numbers
- [`docs/DEPLOYMENT.md`](../DEPLOYMENT.md) — running it on a fleet
- [`zensight-common/docs/data-model.md`](../../zensight-common/docs/data-model.md)
  — `self_stats` and the `sensor-budget` rule
