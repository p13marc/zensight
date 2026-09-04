# ZenSight systemd units

One unit per sensor / exporter (plus the correlator, the historian and the
fleet policy compiler). These units ship inside each
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

**Every other unit holds none, and now says so.** Until #670 they simply left
`CapabilityBoundingSet` unset, which is not "none" — it is the kernel default,
the *full* set. Nothing could use those capabilities (`DynamicUser` with no
`AmbientCapabilities` means an empty effective set), but the bounding set is
what a compromised process could regain, and what `NoNewPrivileges=yes` alone
does not close. Each of those units now carries an explicit empty
`CapabilityBoundingSet=` with the reason next to it, and no unit is above 6.0:

```
$ for f in packaging/systemd/*.service; do
    systemd-analyze security --offline=true "$f" | tail -1
  done | sort -k1

5.6   correlator, desired, both exporters, gnmi, modbus, netflow, snmp,
      sysinfo, systemd, hostspec (ProtectHome=read-only — an operator may
      assert on /home paths; everything hostspec reads, it reads read-only,
      and it executes nothing)
5.6   probe, pve      (empty set; both are clients — nothing on the host to reach)
5.7   parallax        (empty set, plus DeviceAllow — see below)
5.8   logs            CAP_NET_BIND_SERVICE
5.8   netring         CAP_NET_RAW + CAP_IPC_LOCK
5.9   netlink         CAP_NET_ADMIN (+ CAP_BPF CAP_PERFMON)
```

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

Device access costs nothing in that score; an unrestricted capability bounding
set costs 2.3. Eight of the other units still leave theirs unrestricted.

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

`zensight-sensor-container` runs as root (see *Privileges*) and does not score in
the band above; `systemd-analyze security` puts it around 8, which is the honest
number for a process that reads a root-owned socket.
