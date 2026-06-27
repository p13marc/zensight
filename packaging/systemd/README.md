# ZenSight systemd units

One unit per sensor / exporter. The `.deb` / `.rpm` packages install the matching
unit to `/lib/systemd/system/` and an example config to `/etc/zensight/<name>.json5`
(a conf-file, so your edits survive upgrades). Units are **not** enabled
automatically — enable the ones you want:

```bash
sudoedit /etc/zensight/sysinfo.json5          # point it at your Zenoh router
sudo systemctl enable --now zensight-sensor-sysinfo
journalctl -u zensight-sensor-sysinfo -f
```

## Privileges

Every unit runs unprivileged under a transient `DynamicUser` with a minimal
sandbox (`ProtectSystem=strict`, `NoNewPrivileges`, read-only `/etc/zensight`).
Two need extra capabilities, granted as *ambient* caps (still no root):

| Unit | Capability | Why |
|------|-----------|-----|
| `zensight-sensor-netring` | `CAP_NET_RAW` (+`CAP_IPC_LOCK`) | live AF_PACKET / AF_XDP capture (drop for pcap-replay-only) |
| `zensight-sensor-logs` | `CAP_NET_BIND_SERVICE` | bind the privileged syslog port 514 |

`zensight-sensor-netlink` reads kernel state over RTNETLINK / sock_diag
**unprivileged** — no capabilities required.

## Graceful stop

All units stop with `SIGTERM` (`TimeoutStopSec=20s`), which lets a sensor publish
its offline status and tombstone any firing alerts before exit (see #161).
