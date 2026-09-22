# ZenSight systemd units

One unit per sensor / exporter (plus the correlator, the historian and the
fleet policy compiler), and one for a binary that is not ours:
`zenoh-bridge-remote-api.service` runs the upstream zenoh remote-api bridge
that puts a browser on the bus (#705, `docs/DEPLOYMENT.md` §9) — install
that binary with `cargo install`, not from the tarball. These units ship inside each
release's `zensight-<ver>-linux-amd64.tar.gz` (deb/rpm packaging was retired with
the move to Forgejo releases) — install by hand:

```bash
sudo install -m 755 zensight-sensor-sysinfo /usr/local/bin/
sudo install -m 644 systemd/zensight-sensor-sysinfo.service /etc/systemd/system/
# the units say /usr/bin (the old packaged path) — point ExecStart at
# /usr/local/bin, or install the binaries to /usr/bin instead
sudo install -D -m 644 configs/sysinfo.json5 /etc/zensight/sysinfo.json5
sudoedit /etc/zensight/sysinfo.json5          # point it at your Zenoh router
sudo systemctl daemon-reload
sudo systemctl enable --now zensight-sensor-sysinfo
journalctl -u zensight-sensor-sysinfo -f
```

## Configuring them: one policy, not one file per host

Each unit reads its own config from `/etc/zensight`. What belongs there is the
**local** half only: the Zenoh endpoint, credentials, TLS material, and the
`desired.enabled` kill switch — the never-list is exactly the set of things
that must never travel over the bus, because one bad publish would otherwise
lock the fleet out of its own supervision.

Everything an operator authors *about* a host — thresholds, hostspec
assertions, systemd and netlink expectations, log sentinel rules — belongs in
one `fleet-policy.json5`, compiled by `zensight-desired.service` and reconciled
by each sensor over its file baseline. See
[`docs/DEPLOYMENT.md` §6](../../docs/DEPLOYMENT.md#6-day-two-one-policy-file-instead-of-eighteen).

Run **one** `zensight-desired` per deployment: `@desired` is a single-writer
service origin.

## Privileges

Every unit but one runs unprivileged under a transient `DynamicUser` with a
minimal sandbox (`ProtectSystem=strict`, `NoNewPrivileges`, read-only
`/etc/zensight`). The exception is **`zensight-sensor-container`**, which runs
as root on purpose: the rootful podman/docker socket is root-owned and a
`DynamicUser` cannot be granted a stable group to reach it — the unit says so
in its own comments, and it keeps the rest of the sandbox. Some units need
extra capabilities, granted as *ambient* caps (still no root):

| Unit | Capability | Why |
|------|-----------|-----|
| `zensight-sensor-netring` | `CAP_NET_RAW` (+`CAP_IPC_LOCK`) | live AF_PACKET / AF_XDP capture (drop for pcap-replay-only) |
| `zensight-sensor-logs` | `CAP_NET_BIND_SERVICE` | bind the privileged syslog port 514 |
| `zensight-sensor-netlink` | `CAP_NET_ADMIN` (+`CAP_BPF CAP_PERFMON`) | *optional* collectors only — nftables/conntrack + the XFRM monitor (`CAP_NET_ADMIN`) and the eBPF module (`CAP_BPF`/`CAP_PERFMON`, also needs a `--features ebpf` build) |

**Every unit now carries a `MemoryMax`** (#1092). Sixteen of the twenty had
none — including the two that declare a memory budget, so `docs/ops/SIZING.md`'s
rule *"set the budget below `MemoryMax` so the ladder gets to act first"* was
satisfied by **zero** shipped units. The value matches the quadlet twin's
exactly, and `scripts/packaging-check.sh` (run by `ci.yml`'s `lint` job) fails
if the two disagree or if a budget climbs above its backstop. The table is in
[`../README.md`](../README.md).

**Every other unit holds none, and now says so.** Until #670 they simply left
`CapabilityBoundingSet` unset, which is not "none" — it is the kernel default,
the *full* set. Nothing could use those capabilities (`DynamicUser` with no
`AmbientCapabilities` means an empty effective set), but the bounding set is
what a compromised process could regain, and what `NoNewPrivileges=yes` alone
does not close. Each of those units now carries an explicit empty
`CapabilityBoundingSet=` with the reason next to it, and no unit is above 6.0:

<!-- generated: scripts/packaging-check.sh --table ; CI fails if this is stale -->

| Unit | Exposure | Capabilities | `MemoryMax` | `budget_rss_mb` |
|---|---|---|---:|---:|
| `zensight-correlator` | 1.7 OK | — | 128M | — |
| `zensight-desired` | 1.7 OK | — | 96M | — |
| `zensight-exporter-otel` | 1.7 OK | — | 128M | — |
| `zensight-exporter-prometheus` | 1.7 OK | — | 128M | — |
| `zensight-historian` | 1.7 OK | — | 320M | 256 |
| `zensight-sensor-bmc` | 1.7 OK | — | 96M | 72 |
| `zensight-sensor-container` | 2.2 OK | — | 64M | 48 |
| `zensight-sensor-gnmi` | 1.7 OK | — | 96M | 72 |
| `zensight-sensor-hostspec` | 1.7 OK | — | 64M | 48 |
| `zensight-sensor-logs` | 1.8 OK | NET_BIND_SERVICE | 256M | 192 |
| `zensight-sensor-modbus` | 1.9 OK | — | 64M | 48 |
| `zensight-sensor-netflow` | 1.7 OK | — | 128M | 96 |
| `zensight-sensor-netlink` | 2.3 OK | BPF, NET_ADMIN, PERFMON | 128M | 96 |
| `zensight-sensor-netring` | 2.0 OK | IPC_LOCK, NET_RAW | 512M | 448 |
| `zensight-sensor-parallax` | 2.0 OK | — | 512M | 384 |
| `zensight-sensor-probe` | 1.7 OK | — | 64M | 48 |
| `zensight-sensor-pve` | 1.7 OK | — | 96M | 72 |
| `zensight-sensor-snmp` | 1.7 OK | — | 128M | 96 |
| `zensight-sensor-sysinfo` | 1.8 OK | — | 256M | 192 |
| `zensight-sensor-systemd` | 1.8 OK | — | 128M | 96 |

<!-- /generated -->

**The remaining seven were closed on 2026-09-20 (#1204).** `hostspec`, `logs`,
`netlink`, `netring`, `parallax`, `systemd` and `sysinfo` — the seventh was
hidden from the issue's own `grep -L MemoryDenyWriteExecute` by a comment that
merely *mentioned* the directive — each carry every line of the block they can
take, and a `# sandbox-exception: <Directive> — <why>` line in the unit for
each they cannot: `ProtectProc` where the sensor reads other processes'
`/proc/<pid>` (netlink's socket→process join, sysinfo's process table, the
systemd sensor's cgroup attribution), `PrivateDevices` where the sensor's
subject *is* a device node (parallax's cameras, modbus's serial line), plus
`AF_NETLINK` for udev hotplug and `AF_PACKET` for capture. The opt-in tiers
(eBPF, YARA, AF_XDP, SMART) say in the unit which line they replace.
`scripts/packaging-check.sh` now fails a unit that has neither the directive
nor the exception — and one that has both, which is a stale exception. The
spread between the best unit and the worst is a stated difference, not an
accident of writing order.

**Nine of the sixteen were closed first in #1248.** `correlator`, `desired`, both
exporters, `historian`, `gnmi`, `netflow`, `snmp` and `modbus` now carry the
same full sandbox block the four newest units had — `PrivateTmp`,
`PrivateDevices`, `ProtectKernelTunables`, `ProtectKernelModules`,
`ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `ProtectProc`,
`RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`,
`LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallFilter`,
`SystemCallArchitectures`, `RestrictAddressFamilies` — and each dropped from
**5.6 to 1.7**. They are all pure socket-and-disk processes; the block costs
them nothing, and the gap was writing order, not requirement.

`modbus` lands at **1.9**, not 1.7, because `PrivateDevices` is deliberately
left off it: the sensor also speaks Modbus RTU over a serial port
(`port: "/dev/ttyUSB0"`), and a private `/dev` holds only
null/zero/full/random/urandom/tty — the port would simply not be there, and an
RTU deployment would start cleanly and read nothing. A TCP-only deployment can
add `PrivateDevices=yes`; an RTU one wants `DeviceAllow=/dev/ttyUSB0 rw`.

**Seven are still on the thin template, each for a reason that needs testing on
real hardware, not reasoning** — this is the part of #1204 that cannot be
closed from a score:

| Unit | What the full block would break |
|---|---|
| `netlink` | `RestrictAddressFamilies` must name **`AF_NETLINK`**; `SystemCallFilter=@system-service` excludes `bpf(2)`, which the `ebpf` feature needs |
| `netring` | **`AF_PACKET`** for capture and `AF_NETLINK`; same `bpf(2)` question for AF_XDP |
| `sysinfo` | **`ProtectProc=invisible`** hides other processes — the process explorer is most of what this sensor is; `bpf(2)` again for its `ebpf` feature |
| `parallax` | **`PrivateDevices=yes`** removes `/dev/video*`; it already carries a narrower `DeviceAllow` |
| `logs` | binds **514**, and reads the journal — `PrivateDevices`/`ProtectProc` interact with both |
| `systemd` | talks to the D-Bus Manager API and has the gated unit-control surface |
| `hostspec` | asserts on operator-chosen paths, so every `Protect*` is a potential false failure |

Each of those is one line that makes the sensor **start cleanly and collect
nothing**, which is the failure this repository is most careful about — so they
want a host, not an argument. The score above is generated by
`scripts/packaging-check.sh --table` and checked by CI, so a unit that loses
hardening shows up as a stale-table diff.

Two of them carry a caveat in the unit rather than just a reason:

- **`zensight-sensor-snmp`** — the shipped `configs/snmp.json5` binds its trap
  listener to `0.0.0.0:`**`162`**, a privileged port. It is `enabled: false` by
  default, so nothing breaks as shipped; enabling it needs **both**
  `AmbientCapabilities=CAP_NET_BIND_SERVICE` and the matching
  `CapabilityBoundingSet=`, exactly as the logs unit does for syslog on 514 — or
  a port above 1024. The unit could not bind 162 before this change either; it
  granted no ambient capability then and does not now.
- **`zensight-sensor-sysinfo`** — its eBPF `CapabilityBoundingSet` line is
  commented out for the default build. The empty line and that one are
  *alternatives*: an eBPF build replaces one with the other. Leaving the
  commented line as the only mention is what kept this unit at 8.1 while its
  siblings sat lower.

`zensight-sensor-netlink`'s **baseline** reads (interfaces/routes/neighbors/
addresses/sockets/ethtool/tc/diagnostics/RTNETLINK events/XFRM SA dump) are
**unprivileged**. Its shipped unit grants the caps above so a "just run" demo
lights up every collector; drop the `AmbientCapabilities`/`CapabilityBoundingSet`
lines (and re-disable `collect.nftables`/`conntrack`) to return to the pure
unprivileged baseline.

## `zensight-sensor-parallax`: devices, not capabilities

parallax is the one unit that diverges from the template in a direction other
than capabilities — it needs **device access**, and no capabilities at all
(#411). Each is in the unit next to the reason for it:

| Directive | Why |
|---|---|
| `SupplementaryGroups=video` | `/dev/video*` nodes are `root:video 0660`. A `DynamicUser` cannot open one without the group, and `enumerate_v4l2` (on by default) probes `/dev/video0`…`63` by opening each. |
| `DeviceAllow=char-video4linux rw` | Grants the video4linux character devices. Naming *any* `DeviceAllow` switches `DevicePolicy` to `closed`, so this also takes away every other device node the sibling units still reach — a net tightening. |
| `CapabilityBoundingSet=` (empty) | V4L2 capture and RTSP are unprivileged, so parallax needs none. |

The empty bounding set is why it scores *better* than its siblings rather than
worse, which is not the outcome "this one needs the camera" suggests:

```
$ systemd-analyze security --offline=true packaging/systemd/zensight-sensor-parallax.service
parallax  5.7 MEDIUM      # no capabilities at all
netring   5.8 MEDIUM      # CAP_NET_RAW + CAP_IPC_LOCK
logs      5.8 MEDIUM      # CAP_NET_BIND_SERVICE
```

(Re-measured 2026-09-09; these three are unchanged.)

Device access costs nothing in that score; an unrestricted capability bounding
set costs 2.3 — which is why every unit here carries an explicit
`CapabilityBoundingSet=` line, as the section above says. (This paragraph used
to end *"Eight of the other units still leave theirs unrestricted"*, which #670
had already made false sixty-nine lines earlier in this same file.)

**Screen capture is not supported by this unit, and cannot be.** A screen source
would go through the XDG desktop portal, which needs an interactive session
bus and a user consent prompt — neither exists under a system unit with
`DynamicUser`. It would have to be a per-user (`systemd --user`) deployment
variant. Note this is forward-looking: `zensight-sensor-parallax` has no screen
source today (`auto` / `v4l2` / `rtsp` / `test` are the only kinds), so nothing
is being taken away here.

**RTSP** needs ordinary network egress, which is why there is no
`PrivateNetwork=` — the same as every other sensor.

## Graceful stop

All units stop with `SIGTERM` (`TimeoutStopSec=20s`), which lets a sensor publish
its offline status and tombstone any firing alerts before exit (see #161). Every
unit's `ExecStart` names `/usr/bin` — the three 0.13.0 sensors said
`/usr/local/bin` for one release, which was exactly the "unit that fails at exec"
the note at the top of this file is about.

`zensight-sensor-container` runs as root (see *Privileges*). It is in the table
above at **2.2 OK** — better than sixteen of the twenty, because it carries the
full sandbox block; running as uid 0 costs it only what `DynamicUser` would
have saved, which is the honest number for a process that has to read a
root-owned socket.
