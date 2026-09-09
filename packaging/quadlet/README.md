# Per-sensor Quadlet units — the fleet unit (#813)

The all-in-one `zensight-sensors` bundle is a **demo**. A fleet runs one
container per sensor, because the bundle's one cgroup means one `MemoryMax`
pool that the greediest sensor spends for everyone — and on 2026-08-17 that
meant the OOM victim's exit took down the four sensors that would have
explained the incident.

**One `.container` file per unit — all twenty of them since #1093**: fifteen
sensors, the historian, the correlator, `zensight-desired` and both exporters.
Until then there were eleven, and the five proxy sensors most likely to run as
containers on one box (gnmi, modbus, netflow, snmp) plus parallax and the whole
service tier had none, while this sentence already claimed otherwise.
`scripts/packaging-check.sh` — run by `ci.yml`'s `lint` job — fails if a
`.service` ever loses its twin again. Each unit has:

- **its own `MemoryMax`**, sized to that sensor (the placeholders below are
  the reference fleet's starting points — measure yours with
  `just fleet-sizing`, which reads exactly the `self_stats` those numbers
  should have come from; see [`docs/ops/SIZING.md`](../../docs/ops/SIZING.md));
- its own `Restart=on-failure` policy;
- its own on/off switch (`systemctl enable/disable`).

Install: copy the `.container` files you want into
`/etc/containers/systemd/`, adjust `MemoryMax` and the image tag, then
`systemctl daemon-reload && systemctl start zensight-sensor-<name>`.
A host that needs only the basics runs sysinfo + systemd + logs under a cap
sized for those three; netring runs only where it earns its keep.

- **`DropCapability=ALL`**, and then exactly the capabilities its `.service`
  twin grants — nothing more.

The per-unit images are built and pushed by every release
(`zensight-sensor-<name>:<tag>`, plus `zensight-correlator`,
`zensight-historian`, `zensight-desired` and both exporters).

**Do not read capabilities off the bundle's rows in `docs/DEPLOYMENT.md`** —
that is how netring's quadlet acquired a `NET_ADMIN` its unit deliberately
withholds. The bundle's `--cap-add` list is the union across six sensors; a
per-sensor unit needs its own row. The authority is
[`../README.md`](../README.md)'s table, which `scripts/packaging-check.sh`
generates from the units themselves.

**`DropCapability=ALL` is load-bearing and was missing from all eleven units
until #1092.** A quadlet that declares no capability is *not* the equivalent of
its twin's `CapabilityBoundingSet=`: it gets podman's eleven-capability default
(`CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `NET_BIND_SERVICE`,
`SETFCAP`, `SETGID`, `SETPCAP`, `SETUID`, `SYS_CHROOT`). Every unit here was
therefore more privileged than its native form — and the logs quadlet appeared
to work only because `NET_BIND_SERVICE` happened to be in that default.

## Configuring them: one policy, not one file per host

These units mount `/etc/zensight` read-only and each sensor reads its own
config from it — which is the eighteen-files problem at container scale. What
belongs in those files is the **local** half only: the Zenoh endpoint,
credentials, TLS material, and the `desired.enabled` kill switch.

Everything an operator authors *about* a host — thresholds, hostspec
assertions, systemd and netlink expectations, log sentinel rules — belongs in
the fleet policy, compiled by `zensight-desired` and reconciled by each sensor
over its file baseline. See
[`docs/DEPLOYMENT.md` §6](../../docs/DEPLOYMENT.md#6-day-two-one-policy-file-instead-of-eighteen)
and [`zensight-desired/docs/policy.md`](../../zensight-desired/docs/policy.md).

`state/<producer>/applied/<topic>` on each host says which writer won last —
`file`, `desired` or `rpc` — which is where to look when a change does not
appear to have taken.
