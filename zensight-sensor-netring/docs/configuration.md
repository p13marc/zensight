# netring configuration

JSON5, loaded from `--config <file>`. The full annotated example is
[`configs/netring.json5`](../../configs/netring.json5); this page documents each
block and its defaults, sourced from `src/config.rs`. Top-level keys: `zenoh`,
`serialization`, `logging`, `artifacts`, and `netring` (all the sensor-specific
settings).

Validation (`NetringSensorConfig::validate`) requires **either** at least one
`netring.interfaces` entry **or** `netring.pcap`; and `capture.to_disk.dir` when
`capture.to_disk.mode` is not `off`.

---

## Capture source

```json5
netring: {
  key_prefix: "zensight/netring",   // legacy-form prefix; derives the v1 context (base + producer)
  source: "auto",                   // payload `source` field (not in the key); "auto" → hostname
  backend: "auto",                  // "auto" | "afpacket" | "afxdp"
  interfaces: ["eth0"],             // live capture NICs (needs CAP_NET_RAW)
  // pcap: "/path/to/capture.pcap", // replay instead (no privileges); always overrides `interfaces`
}
```

- **`backend`** (#227): `auto` (default) probes the host/interface and picks the
  best available backend, logging the choice (`AfXdp` needs an AF_XDP-enabled
  build, which this sensor doesn't compile — it falls back to `auto` → AF_PACKET
  with a warning). `afpacket` forces AF_PACKET (TPACKET_v3). `pcap` always
  overrides.
- Multiple `interfaces` are each captured per-NIC under AF_PACKET. A
  `capture-leg-asymmetry` alert fires if a flow's two directions arrive on
  mismatched capture legs (tap miswire).

## `collect.*` — collectors

Each toggles a telemetry family (see [telemetry.md](telemetry.md)). Defaults:

| Key | Default | What it collects |
|---|---|---|
| `bandwidth` | **on** | per-application bytes/sec |
| `flows` | **on** | flow lifecycle + volume + RED aggregates |
| `tcp_resets` | **on** | TCP reset + connection-refused counters |
| `tls` | **on** | passive TLS fingerprinting (SNI + JA3/JA4, PQ ratio) |
| `capture_stats` | **on** | capture self-health (packets/drops/drop_rate); live-only |
| `talkers` | **on** | top-talkers + matrix + elephant-flow detail procedures (#21/#122) |
| `infer_initiator` | **on** | TCP SYN-analysis to label flows client → server regardless of capture order (#122); TCP-only, zero cost when off |
| `icmp` | off | ICMP error telemetry (live-gated; silent under replay) |
| `dns` | off | L7 DNS RED (cleartext UDP/53) — needed by passive DNS + DNS detectors |
| `http` | off | L7 HTTP RED (cleartext TCP/80,8080; TLS is opaque) |
| `quic` | off | passive QUIC Initial SNI/ALPN/version/JA4 (UDP/443) |
| `ssh` | off | SSH banner + client/server HASSH + KEXINIT (TCP/22) |
| `encrypted_dns` | off | classify DoT/DoQ/DoH from the handshake (#326) |
| `ip_reassembly` | off | reassemble IP fragments before L7 parsing (#326) |
| `http_fp` | off | JA4H HTTP fingerprints (#124); no-op unless `--features ja4plus` |
| `snmp_cleartext` | off | flag cleartext SNMP v1/v2c communities; no-op unless `--features snmp` |
| `assets` | off | passive asset inventory (ARP/NDP/LLDP) |
| `asset_cdp` | off | also feed the inventory from CDP (forces capture-all prefilter); needs `assets` |
| `ipfix` | off | canonical IPFIX flow export on `@rpc/netring/ipfix` (#223); no-op unless `--features ipfix` |

## `anomalies.*` — detectors

All enables are **off by default**; thresholds carry documented defaults even
when the detector is off. See [detectors.md](detectors.md) for behaviour and
ATT&CK mapping.

| Key | Default | Notes |
|---|---|---|
| `port_scan` | off | TRW port-scan |
| `beaconing` / `beacon_threshold` | off / 0.8 | CV beaconing |
| `rita_beacon` / `rita_beacon_threshold` | off / 0.9 | robust RITA beaconing (threshold shared with the FQDN variant) |
| `rita_beacon_fqdn` | off | FQDN-pivoted RITA beacon (needs `collect.dns` + `names`) |
| `dns_tunnel` / `dns_tunnel_distinct` / `dns_tunnel_qname_len` | off / 50 / 100 | needs `collect.dns` |
| `nod` | off | Newly-Observed-Domain (needs `collect.dns`) |
| `dga` / `dga_threshold` | off / −8.0 | needs `collect.dns` |
| `connection_flood` / `flood_threshold` | off / 100 | |
| `lateral_movement` | off | needs `--features lateral` |
| `data_exfil` / `exfil_sigma` / `exfil_min_bytes` | off / 4.0 / 10 MiB | needs `collect.flows` |
| `encrypted_dns_bypass` / `dns_resolver_allowlist` | off / `[]` | needs `collect.encrypted_dns` (#326) |
| `allowlist` | `[]` | case-insensitive substring suppression for noisy detectors |

## `threat.*` — threat-intel

```json5
threat: {
  flow_risk: false,                 // obsolete TLS, cleartext HTTP credentials
  ioc: { ips: [], domains: [], ja4: [], ja3: [], files: [] },
  sigma: { enabled: false, dir: null },  // needs --features sigma
  yara: { file: null },             // needs --features yara
  reload: false,                    // always arm the IOC/YARA matchers for @rpc/netring/threat_intel/set
}
```

Set `reload: true` (or provide startup indicators / a `yara.file`) to hot-reload
IOC / YARA at runtime — otherwise the frozen-at-build matchers make a reload a
no-op. See [detectors.md](detectors.md#runtime-threat-intel-reload--rpcnetringthreat_intel-328).

## `overload` — capture-overload detection

Watches the windowed drop-rate with Suricata-style hysteresis; raises/clears the
`capture-overload` SensorHealth alert. No-op without `collect.capture_stats`.

```json5
overload: {
  enabled: true,
  enter_drop_rate: 0.05,    // enter Emergency at 5%
  recover_drop_rate: 0.01,  // recover below 1% ...
  recover_windows: 3,       // ... sustained for 3 calm windows
  shed: {                   // active load-shedding (#224), off by default
    enabled: false,
    policy: "new_flows",    // "new_flows" (drop all new) | "sample"
    sample_rate: 0.5,       // fraction of new flows kept under "sample"
  },
}
```

With `shed.enabled` on, while in Emergency the sensor **deliberately** sheds new
flows at the dispatch boundary (honest, counted drops via `capture/<src>/shed/*`
+ a `shedding` label on the alert) instead of opaque kernel loss; already-tracked
flows keep flowing (elephant-friendly). Detection (`enabled`) must be on for
shedding to fire.

## `capture_focus` — runtime capture focus (#225)

Off by default (the packet-tier handler is a per-frame cost the default build
avoids). When on, registers a reloadable packet subscription whose BPF filter can
be hot-swapped at runtime via the `@rpc/netring/capture_filter/set` procedure
(read the current filter on `@rpc/netring/capture_filter`; no capture restart)
to narrow attention during an incident; `capture/focus/{packets,bytes}` counters
show the effect.

```json5
capture_focus: {
  enabled: false,
  base_expr: "tcp or udp or icmp",  // netring .expr() grammar; permissive base
}
```

Grammar: `tcp|udp|icmp`, `[src|dst] port N`, `[src|dst] host IP`, `[src|dst] net
CIDR`, combined with `and`/`or`/`!`/parens.

## `bandwidth_period_secs` / `bandwidth_attribution`

```json5
bandwidth_period_secs: 5,        // per-app bandwidth + wire-attribution cadence
bandwidth_attribution: false,    // opt-in per-process WIRE bandwidth (#318)
```

`bandwidth_attribution` (off by default) joins netring's live per-flow wire
bandwidth against the kernel socket table (periodic sock_diag dump + `/proc` fd
scan, off the hot path) and serves per-process **wire-L2** rows query-only on
`@rpc/netring/bandwidth`. Best-effort; costs `/proc` walks; reads are unprivileged.
See [telemetry.md](telemetry.md#per-process-wire-bandwidth-318-opt-in).

## `names` — passive-DNS name cache (#308)

Enabled by default but **inert without `collect.dns`**. Observes DNS answers into
a bounded IP → name map (flowscope `NameMap`) that enriches flow/talker records
and keys the FQDN beacon detector. Every cap is bounded so a CDN-heavy network
can't grow the map without limit.

```json5
names: {
  enabled: true,
  max_ips: 16384,           // LRU cap on distinct IPs
  max_claims_per_ip: 8,     // provenance-tagged names kept per IP
  default_ttl_secs: 300,    // TTL for claims whose source carries none
  grace_secs: 60,           // added to every TTL before expiry
  batch_interval_secs: 10,  // new-mapping delta-feed drain cadence
  max_batch: 500,           // pending delta-feed cap between drains
}
```

## `evidence` — host-evidence feed (#307)

Republishes observed assets / passive-DNS names as identity evidence on
`zensight/v1/<origin>/state/netring/evidence/{device/<device>,names/<ip-slug>}`
for the correlator. Enabled by default but **gated on its source collectors**:
asset evidence needs `collect.assets`, name evidence needs `collect.dns` — with
those off (the shipped default) netring emits no evidence even though `enabled`
is true.

```json5
evidence: {
  enabled: true,
  min_interval_secs: 60,   // floor between drain ticks (caps churn on a busy L2 segment)
  refresh_secs: 420,       // re-emit live claims at least this often (≤ ttl/2)
  max_per_tick: 200,       // hard cap on records per tick (remainder carried over)
}
```

## `capture` — on-demand & to-disk packet capture

Nested: `on_demand` (operator-pulled artifact) and `to_disk` (continuous spool /
triggered ring). Both off by default. Arming a tap adds a build-time packet
subscription (a per-frame predicate eval).

### `capture.on_demand` (#333)

```json5
on_demand: {
  enabled: false,
  max_duration_secs: 300,   // hard cap; longer requests clamped
  max_bytes: 268435456,     // 256 MiB; capture stops early (truncated) at the cap
  snaplen_max: 0,           // 0 = full frames allowed
  allow_filter: true,       // accept per-request narrowing filters
  cooldown_secs: 60,        // min gap between captures (rate limit)
  ttl_secs: 600,            // how long a finished capture stays fetchable
  chunk_size: 524288,       // 512 KiB blob-transfer chunk
}
```

When `enabled`, an operator pulls a bounded `pcap[.zst]` via the unified
artifact procedures (`@rpc/netring/artifact/request` + `artifact/cancel`,
lifecycle doc at `state/netring/artifact/capture`; GUI *Capture* tab or any
client), delivered as a Tier-1 blob on
`zensight/v1/<origin>/@blob/artifact/...`. Every request is clamped to these limits. **Limitation:** the packet tier
only sees IP/L4 frames — non-IP traffic (ARP/LLDP) is not captured. Backpressure
is drop-with-count (a lossy capture never stalls telemetry).

### `capture.to_disk` (#327)

```json5
to_disk: {
  mode: "off",                     // "off" | "rotating" | "triggered"
  // dir: "/var/lib/zensight/captures",  // REQUIRED when mode != off (writable; systemd ReadWritePaths=)
  ring_bytes: 33554432,            // 32 MiB pre-trigger ring (triggered — hard RSS cost while armed)
  max_file_bytes: 67108864,        // 64 MiB per file (rotation trigger / triggered hard stop)
  rotate_secs: 0,                  // 0 = size-based rotation only (rotating)
  max_files: 16,                   // retention: file count (oldest evicted)
  max_total_bytes: 1073741824,     // retention: 1 GiB total
  snaplen: 0,                      // 0 = full frames
  trigger_min_severity: "warning", // "info" | "warning" | "critical"
  trigger_kinds: [],               // detector slugs; empty = all anomalies
  post_trigger_secs: 10,           // aftermath recorded after the trigger
  compress: true,                  // zstd the finished trigger file
  artifact_ttl_secs: 3600,         // how long the blob stays downloadable
}
```

- **`rotating`** — continuously spool rotating pcap files to `dir` (local
  forensics; metadata-only on the bus, indexed on `@rpc/netring/captures`).
- **`triggered`** — buffer recent frames in the in-memory pre-trigger ring;
  when an anomaly at/above `trigger_min_severity` fires (optionally narrowed to
  `trigger_kinds`) or the GUI sends `capture_now`, flush the lead-up +
  `post_trigger_secs` of aftermath to a `pcap[.zst]` served as a TTL'd Tier-1
  blob.
- Control at runtime via the `@rpc/netring/capture_disk/set` procedure
  (`capture_now`, `set_capture`; status read on `@rpc/netring/capture_disk`);
  a mode that is `off` at startup is not armed, switching between armed modes is
  live. Health rides `capture/disk/*` + `capture/events`.

## `artifacts` — report / snapshot channels

Top-level (not under `netring`); the framework-wide artifact procedures
(`@rpc/netring/artifact/{request,cancel}` + `state/netring/artifact/<kind>`
status docs, bytes on the `@blob` plane) for Tier-1 `report` (redacted debug
bundle) and Tier-2 `snapshot` (allowlisted directory — a natural fit for a
captured-pcap directory). Every kind disabled by default; the on-demand pcap
`Capture` kind is configured under `netring.capture.on_demand` instead. See
`docs/LARGE-DATA-TRANSFER.md`.
