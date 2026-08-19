# ZenSight systemd units

One unit per sensor / exporter (plus the correlator). These units ship inside each
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

## Privileges

Every unit runs unprivileged under a transient `DynamicUser` with a minimal
sandbox (`ProtectSystem=strict`, `NoNewPrivileges`, read-only `/etc/zensight`).
Some units need extra capabilities, granted as *ambient* caps (still no root):

| Unit | Capability | Why |
|------|-----------|-----|
| `zensight-sensor-netring` | `CAP_NET_RAW` (+`CAP_IPC_LOCK`) | live AF_PACKET / AF_XDP capture (drop for pcap-replay-only) |
| `zensight-sensor-logs` | `CAP_NET_BIND_SERVICE` | bind the privileged syslog port 514 |
| `zensight-sensor-netlink` | `CAP_NET_ADMIN` (+`CAP_BPF CAP_PERFMON`) | *optional* collectors only — nftables/conntrack + the XFRM monitor (`CAP_NET_ADMIN`) and the eBPF module (`CAP_BPF`/`CAP_PERFMON`, also needs a `--features ebpf` build) |

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
its offline status and tombstone any firing alerts before exit (see #161).
