# Per-sensor Quadlet units — the fleet unit (#813)

The all-in-one `zensight-sensors` bundle is a **demo**. A fleet runs one
container per sensor, because the bundle's one cgroup means one `MemoryMax`
pool that the greediest sensor spends for everyone — and on 2026-08-17 that
meant the OOM victim's exit took down the four sensors that would have
explained the incident.

One `.container` file per sensor, each with:

- **its own `MemoryMax`**, sized to that sensor (the placeholders below are
  the reference fleet's starting points — measure yours via each sensor's
  health doc `self_stats`);
- its own `Restart=on-failure` policy;
- its own on/off switch (`systemctl enable/disable`).

Install: copy the `.container` files you want into
`/etc/containers/systemd/`, adjust `MemoryMax` and the image tag, then
`systemctl daemon-reload && systemctl start zensight-sensor-<name>`.
A host that needs only the basics runs sysinfo + systemd + logs under a cap
sized for those three; netring runs only where it earns its keep.

The per-sensor images are built and pushed by every release
(`zensight-sensor-<name>:<tag>`); mounts/capabilities per sensor are the
same as the bundle's rows in `docs/DEPLOYMENT.md`.
