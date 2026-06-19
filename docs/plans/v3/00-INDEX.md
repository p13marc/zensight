# ZenSight v3 — Sensors + GUI roadmap

A fresh deep-analysis round (after the GUI overhaul and the conntrack/WireGuard/
TLS/capture sensor features landed). Built from five investigations: unused
capability inventories of the three sensors against the pinned `sysinfo`/`nlink`/
`netring`+`flowscope` crates, a GUI gap audit, and citation-backed web research on
host‑metrics (USE), kernel/network observability, NDR/flow analytics, L7 (RED),
and desktop monitoring UX.

> Scope: `zensight-sensor-{sysinfo,netlink,netring}` and the `zensight` GUI.

## The plans

| # | Plan | Theme |
|---|------|-------|
| [01](01-sysinfo-sensor.md) | sysinfo sensor | PSI, vmstat saturation, cgroup‑v2, fd/inode ceilings, per‑process, NIC drops, thermal/power |
| [02](02-netlink-sensor.md) | netlink sensor | TC/QoS drops, ethtool link/ring, address inventory, real‑time netlink events, richer sockets, XFRM, nftables |
| [03](03-netring-sensor.md) | netring sensor | L7 DNS (RED) + HTTP, RITA‑style beaconing, more NDR detectors, ICMP errors, per‑proto + top‑talkers, elephant flows |
| [04](04-gui.md) | GUI | local tiered time‑series store, real topology edges, alert timeline/silence/notifications, freshness, trend badges, search, favorites, D2/L5 cleanup, keyboard help |
| [05](05-architecture-and-conventions.md) | **Architecture & conventions** | cross‑cutting contract: idioms, strong typing, async patterns, performance — every 01–04 commit follows it |

## The converged "first wave" (research‑backed, highest signal)

The five research streams converged on a coherent first wave — each item is the
highest‑signal/evasion‑resistant choice in its area and maps cleanly onto
ZenSight's existing `TelemetryPoint`/alert model:

- **host →** Pressure Stall Information (`/proc/pressure/*`) + the vmstat
  saturation allowlist (`oom_kill`, `pgmajfault`, `pswpin/out`) + FD/inode
  ceilings. *Saturation, not averaged utilization, is the under‑collected
  high‑signal dimension (USE method).*
- **kernel →** event‑driven RTNETLINK (link/addr/route up‑down) instead of
  polling, + reason‑labeled drop counters (conntrack %, ethtool ring drops, qdisc
  backlog).
- **NDR →** RITA‑style **beaconing** scoring + **JA4** TLS inventory (we already
  have JA4 capture — beaconing is the gap). The two highest‑SNR, evasion‑resistant
  passive detections.
- **L7 →** DNS **RED** analytics (NXDOMAIN/SERVFAIL rate, query‑RTT p50/p95/p99,
  unanswered rate, top domains). DNS is unencrypted → the highest‑signal fully
  passive L7. (HTTP cleartext is a second tier.)
- **UX →** honest per‑panel **freshness** (Live/Stale/Paused + "as of HH:MM")
  backed by a **tiered local ring‑buffer store** (Netdata‑style), so trends
  survive restart. Trustworthy liveness is the foundation every other feature
  depends on.

## Recommended sequencing

```
Wave 1 (highest signal, mostly low effort):
  01 PSI+vmstat+fd/inode · 02 ethtool+addresses+events · 03 ICMP+per-proto+beaconing · 04 freshness+local store
Wave 2 (depth):
  01 cgroup-v2+per-process · 02 TC/QoS+richer sockets · 03 DNS RED · 04 real topology edges + trend badges
Wave 3 (polish/specialized):
  02 XFRM/nftables · 03 HTTP L7 + more detectors · 04 alert timeline/silence/notifications, search, favorites, D2/L5, keyboard help
```

## Cross-cutting principles (carried from v1/v2 + research)

- **USE** for resources (host/links): utilization **+ saturation + errors** —
  prioritize the saturation/error signals (PSI, drops, OOM) that averages hide.
- **RED** for services (DNS/HTTP/flows): rate, errors, duration (latency as a
  p50/p95/p99 distribution, not a mean; split success vs failure).
- **Cardinality discipline (P2):** stream low‑cardinality aggregates; serve
  high‑cardinality detail (per‑flow, per‑socket, per‑rule, top‑talkers) via
  `@/query/*` channels.
- **Detect at the edge (P1):** the sensor emits alerts/anomalies; pair every
  noisy detector (beaconing/exfil) with a known‑good allowlist.
- **Privilege‑honest:** root‑gated features (conntrack, WireGuard, ethtool set,
  capture) degrade gracefully and are config‑gated; the sensor still runs
  unprivileged.
- **Verification discipline (carried):** pure functions unit‑tested; live‑verify
  where the sandbox allows (unprivileged netlink reads, pcap replay, real
  in‑proc queryable); for root/live‑only paths, verify the API against the pinned
  source + unit‑test the mapping + degrade gracefully (the ICMP/DNS‑pcap lessons).

## Verified corrections to the source audits

- Alert grouping + acknowledge UI is **already shipped** (do not re‑plan it).
- netlink + netring dashboard overviews are **already implemented**.
- Topology edges are still a **simulated mesh** (`topology/mod.rs:147` "mesh
  topology for demo") — real adjacency from neighbors/flows is a genuine gap.
- ICMP errors were **backed out** earlier (synthetic pcap couldn't trigger
  `on_icmp_error`); re‑addable as a live‑capture‑gated feature.
- Hardcoded `Color::from_rgb` sites outside theme/tokens: **16** (Plan 04 D2).

## API verification (every cited capability checked against the pinned source)

The sub‑agents' API claims were verified against the pinned crates before being
written into the plans (they occasionally invent plausible method names):

- **nlink** ✓ `subscribe(&[RtnetlinkGroup::…])` + `conn.events().await` returns a
  `Stream` of `NetworkEvent` (→ async via `tokio::select!`); `get_qdiscs/_classes/
  _filters` (TC); ethtool `get_link_state/_link_info/_link_modes/_features/_rings/
  _channels/_pause`; xfrm `get_security_associations/_policies`; nftables
  `list_tables/_flowtables`.
- **flowscope 0.16** ✓ `detect::patterns::{BeaconDetector<K>, DgaScorer}` exist.
- **netring** ✓ `datagram_stream`/`datagrams` are on the **pcap source, not the
  Monitor builder** — confirms Plan 03's DNS feasibility caveat (needs new
  plumbing).
- **procfs 0.17** ✓ PSI (`CpuPressure/MemoryPressure/IoPressure` with
  `PressureRecord{avg10,avg60,avg300,total}`), `KernelStats{ctxt,processes,
  procs_running,procs_blocked}`, `Meminfo`, `cgroups` are typed. **Correction:**
  procfs has **no vmstat** module → the saturation allowlist (`oom_kill`,
  `pgmajfault`, `pswpin/out`) needs a small custom `/proc/vmstat` parser (Plan 01).
