# zensight-sensor-pve

The hypervisor as a hypervisor (#818).

The reference fleet's Proxmox host was monitored by three native binaries —
sysinfo, systemd, logs — reporting CPU, memory, disks, units and the journal.
That is a complete picture of a **Linux box**. Everything that made the machine
a *hypervisor* was invisible, and the machine whose failure is total is the one
you can least afford to see partially.

The 2026-08-28 audit of that fleet found three things by hand, once, weeks
late. Each is now a continuous assertion:

| Found by reading configuration carefully | Asserted here |
|---|---|
| VM 140 had `onboot=0` — it would not have come back after a host reboot | `guest-onboot-off` |
| VM 140's NIC had no `firewall=1`, so `140.fw` was inert and :8000 was open to the whole service zone for an unknown period | `guest-nic-firewall-off` |
| 990 GB provisioned on a 937 GB pool | `pool-overcommitted` |

**None of those is a metric that spikes.** They are configuration facts that
stopped matching intent — invisible to every threshold on every dashboard, and
visible to a poller that checks them on every cycle. That is what this sensor
is for. The gauges exist so guests get device cards, Prometheus gets series and
the family-coverage audit has families; the *output* is the state documents and
the alert set.

## There is no action surface

Not disabled. Absent. Nothing in this crate constructs a non-GET request, the
registry slice declares no `write` procedure, and a test fails if one ever
appears. A monitor that can stop a VM is a different threat model; if one is
ever wanted it must be default-off with an allowlist — the way the systemd
sensor's `actions` block already is — as a separate and deliberate decision.

## What it publishes

| | |
|---|---|
| `state/pve/guest/{vmid}` | runtime status joined with the config facts that decide the *next* reboot: `onboot`, per-NIC `firewall`, per-disk `backup=0`, provisioned size |
| `state/pve/storage/{store}` | capacity, use, and **allocated** — the sum of declared volume sizes, which is what over-commits a thin pool and is invisible in `used` |
| `state/pve/backup/{vmid}` | last vzdump outcome plus the newest two stored volumes, so "it succeeded and shrank" is expressible |
| `state/pve/cluster` | quorum, members, HA resources, replication results |
| `state/pve/evidence/device/{vmid}` | a third-party identity claim per guest — name and configured MACs — so the hypervisor's view of a VM fuses with that VM's own sensors in the catalog |
| `telemetry/pve/…` | per-guest cpu/mem/disk/uptime/running, per-pool bytes and ratios, per-guest backup size/age/duration, cluster counts |
| `state/pve/alert/{alert_key}` | ten rules, each reconciled every sweep |

## Backup size is the point

"The job exited 0" is what the existing mail notification already says.
`backup/{vmid}/size_change_pct` is the one it cannot: a dump that **succeeds
while halving** looks perfect from the exit code and is a restore that will not
work. The comparison baseline comes from the *store*, not from memory, so a
sensor restart does not quietly make the shrunk size the new normal.

## Shape

- **A polling sensor over an HTTP API with per-device liveness** — the same
  shape the SNMP and gNMI sensors already are. Guests are the `<device>`s; the
  origin stays the polling host's.
- **Three cadences**, because runtime status, guest configuration and backups
  move at three speeds and polling all of them at the fastest would be a
  monitoring sensor hammering the machine whose failure is total.
- **A read-only, scoped API token** (`PVEAuditor`), resolved through the
  framework's `file:`/`${ENV}` indirection so the secret never enters a config
  file or git.
- **Bounded**: an explicit concurrency cap, and a timeout that startup refuses
  unless it is shorter than the poll interval.
- **Forgiving of a 403.** `PVEAuditor` legitimately cannot read some endpoints,
  and HA/replication do not exist on a standalone node. Those are facts about
  the install, published as "not applicable", never graded as sensor failure.

## Running it

On the PVE host, as a native binary (the reference deployment's security rules
forbid containers there, and `packaging/systemd/zensight-sensor-pve.service`
ships hardened for exactly this), or from a guest against the API over the
network (`packaging/quadlet/zensight-sensor-pve.container`).

```bash
pveum user add zensight@pve
pveum acl modify / --users zensight@pve --roles PVEAuditor
pveum user token add zensight@pve ro --privsep 0     # note the secret, once

cp configs/pve.json5 /etc/zensight/pve.json5          # set host; token: file:…
install -m 0600 /dev/null /etc/zensight/pve.token     # paste the token in
just pve                                              # or the systemd unit
```

It is deliberately **not** part of `just run`: no demo can invent a Proxmox
endpoint or a credential, and a sensor that crash-loops in the demo teaches the
wrong thing.

## Docs

| | |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | every key, and why each default is what it is |
| [`docs/assertions.md`](docs/assertions.md) | the ten rules, what each catches, and what it deliberately does not |
| [`src/lib.rs`](src/lib.rs) | scope and non-goals |
