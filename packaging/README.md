# Packaging — the two forms, and the table that keeps them honest

ZenSight ships every producer twice, and until #1092 nothing compared the two:

- **`systemd/`** — a native `.service` per binary, for the release tarball's
  `/usr/bin` install path;
- **`quadlet/`** — a `.container` per binary, for the one-container-per-sensor
  fleet deployment (#813), against the per-sensor images every release builds.

Each directory's own README explains its form. This file is the **contract
between them**, and `scripts/packaging-check.sh` — which `ci.yml`'s `lint` job
runs — is what makes it a contract rather than a wish.

## What the check enforces

1. **Both forms grant the same capabilities.** Three units had drifted: the
   netring quadlet granted `NET_ADMIN` its unit deliberately withholds; the
   netlink quadlet omitted `BPF`/`PERFMON`, so eBPF was silently unavailable in
   the container form only; and the logs quadlet declared nothing where its
   unit grants `CAP_NET_BIND_SERVICE`.
2. **Every quadlet carries `DropCapability=ALL`.** This is the deeper half of
   the same defect and the reason the logs quadlet appeared to work. Without
   it, a `.container` that declares no capability is **not** the equivalent of
   `CapabilityBoundingSet=` — it is podman's eleven-capability default
   (`CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `NET_BIND_SERVICE`,
   `SETFCAP`, `SETGID`, `SETPCAP`, `SETUID`, `SYS_CHROOT`). Every quadlet in
   this tree was therefore more privileged than its `.service` twin, including
   the ones whose twin holds nothing at all.
3. **Every unit has a `MemoryMax`,** in both forms, with the same value.
   Sixteen of twenty `.service` files had none.
4. **Every declared budget is below its unit's `MemoryMax`.**
   `docs/ops/SIZING.md` states this invariant — *"set the budget below
   `MemoryMax` so the ladder gets to act first"* — and before #1091/#1092
   **zero** shipped units satisfied it: the only four `.service` files with a
   `MemoryMax` were exactly the four sensors that could not declare a budget.
5. **Every `.service` has a `.container` and vice versa.** #1093 closed the
   nine-unit gap; the check is what stops it reopening.

Run it yourself:

```bash
scripts/packaging-check.sh            # check
scripts/packaging-check.sh --table    # regenerate the table below
```

## The table

Capabilities are the `.service`'s `AmbientCapabilities` without the `CAP_`
prefix; the quadlet's `AddCapability` must match. `budget_rss_mb` is
`resources.budget_rss_mb` from that unit's `configs/*.json5` (#1091).

**Generated — do not hand-edit.** `scripts/packaging-check.sh --table`.

| Unit | Capabilities | `MemoryMax` | `budget_rss_mb` |
|---|---|---:|---:|
| `zensight-correlator` | — | 128M | — |
| `zensight-desired` | — | 96M | — |
| `zensight-exporter-otel` | — | 128M | — |
| `zensight-exporter-prometheus` | — | 128M | — |
| `zensight-historian` | — | 320M | 256 |
| `zensight-sensor-bmc` | — | 96M | 72 |
| `zensight-sensor-container` | — | 64M | 48 |
| `zensight-sensor-gnmi` | — | 96M | 72 |
| `zensight-sensor-hostspec` | — | 64M | 48 |
| `zensight-sensor-logs` | NET_BIND_SERVICE | 256M | 192 |
| `zensight-sensor-modbus` | — | 64M | 48 |
| `zensight-sensor-netflow` | — | 128M | 96 |
| `zensight-sensor-netlink` | BPF, NET_ADMIN, PERFMON | 128M | 96 |
| `zensight-sensor-netring` | IPC_LOCK, NET_RAW | 512M | 448 |
| `zensight-sensor-parallax` | — | 512M | 384 |
| `zensight-sensor-probe` | — | 64M | 48 |
| `zensight-sensor-pve` | — | 96M | 72 |
| `zensight-sensor-snmp` | — | 128M | 96 |
| `zensight-sensor-sysinfo` | — | 256M | 192 |
| `zensight-sensor-systemd` | — | 128M | 96 |

The four service-tier rows have no budget because they have no health document
to carry one — see #1202. Their `MemoryMax` is therefore the only number
holding them, and it cannot be measured with `just fleet-sizing` the way a
sensor's can.

Every `MemoryMax` here is a **starting point**, not a measurement. The two
budgets that came from one — netring's 448 and the historian's 256 — say so in
their own config files. The rest sit at three quarters of their unit's
`MemoryMax`, which is deliberately the same fraction `governor.rs`'s
`CGROUP_BUDGET_FRACTION` uses when it derives a budget from a cgroup, so a
declared budget and a derived one agree.

## See also

- [`systemd/README.md`](systemd/README.md) — the native form, its sandbox, and
  the `systemd-analyze security` scores
- [`quadlet/README.md`](quadlet/README.md) — the container form
- [`docs/ops/SIZING.md`](../docs/ops/SIZING.md) — how to replace these guesses
  with measurements
- [`docs/DEPLOYMENT.md`](../docs/DEPLOYMENT.md) — which of these a deployment runs
