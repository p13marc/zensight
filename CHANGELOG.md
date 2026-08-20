# Changelog

All notable changes to ZenSight will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A one-shot `@rpc` reader, so a queryable can be read without a GUI** (#168).
  `zenctl` lives in the external zenkey repo and the desktop app needs a display,
  so a query channel had no reader at all on a headless host — which is part of
  why the eBPF frontier went a month without on-host validation.
  `cargo run -p zensight-common --example rpc_get -- 'v1/*/@rpc/sysinfo/latency'`
  issues one GET and pretty-prints every reply, exiting non-zero when nobody
  answered. It *connects* where `v1_probe` listens, because a validation run
  starts the sensor first and dialling an already-listening peer skips the
  connect-retry backoff.

- **Every systemd unit now restricts its capability bounding set** (#670). Nine
  of the thirteen left `CapabilityBoundingSet` unset — which is not "none", it
  is the kernel default, the *full* set — and scored 8.1 EXPOSED on
  `systemd-analyze security` against 5.7–5.9 for the four that restricted it.
  Nothing could use those capabilities (`DynamicUser` with no
  `AmbientCapabilities` means an empty effective set), but the bounding set is
  what a compromised process could regain and what `NoNewPrivileges=yes` alone
  does not close. Each now carries an explicit empty set with its reason, and no
  unit is above 6.0. Two carried something worth writing down: the SNMP unit's
  shipped trap-listener bind is the privileged port **162** (default-off, and
  never bindable under this unit — enabling it needs an ambient capability as
  well), and sysinfo's bounding-set line was commented out for the eBPF build,
  which is what left it unrestricted for the default one.

- **A systemd unit for the parallax sensor** (#411).
  `packaging/systemd/zensight-sensor-parallax.service` follows the hardened
  sensor template and diverges only where live video requires it:
  `SupplementaryGroups=video` (device nodes are `root:video 0660`, and
  `enumerate_v4l2` opens `/dev/video0`…`63` to probe them) and
  `DeviceAllow=char-video4linux rw` — which, by naming any device at all,
  switches `DevicePolicy` to `closed` and so takes away every other device node
  the sibling units still reach. It also carries an empty
  `CapabilityBoundingSet=`, because V4L2 capture and RTSP need no capabilities:
  the unit that wants the camera ends up with the *lowest*
  `systemd-analyze security` exposure of any sensor (5.7, against netring's and
  logs' 5.8, which each need one capability). Screen capture is documented as
  not supported by a system unit and not possible under one — it would need the
  XDG portal and an interactive session.

- **CI compiles every optional feature, not one of ten** (#662). Ten features
  across four crates gate `#[cfg(feature = ...)]` code that a default build
  never type-checks; CI built exactly one of them, which is how `h264` stayed
  broken for a week (#485 → #649). A new `features` job checks the eight a
  stable toolchain can reach — `zensight` `tester`/`h264` and netring's
  `sigma`/`yara`/`snmp`/`lateral`/`ipfix`/`ja4plus` — one named step each, so
  the log says which feature broke. `ja4plus` (FoxIO License 1.1, not OSI)
  stays in its own opt-in step and off the default path. The two `ebpf`
  features need nightly + `rust-src` + `bpf-linker`, so they run nightly in a
  separate `eBPF features` workflow rather than on every push.

- **A trap record names the alert it raised or cleared** (#651). `EventRecord`
  gains `alert_key: Option<String>` (serde-default and skipped when absent, so
  old records decode and records that drove no alert transition are unchanged on
  the wire). The SNMP trap path stamps the key the reporter actually published
  under — computed from the same `Alert`, so the two cannot drift — and a
  clearing trap names the alert it cleared rather than nothing.
  The SNMP event feed links straight to that alert instead of pivoting to the
  device's alert list, which is the difference between landing on an incident
  and landing in a list when several alerts fire on one device. Records without
  the field keep the source-scoped pivot. The Alerts view marks the linked row
  and, if that alert has since resolved, says so with its firing→resolved
  timeline rather than showing an empty list.
  - Alert transitions now publish **before** the event record that references
    them, so a consumer never sees a record pointing at an alert it has not
    ingested.
- **A shipped Zenoh storage config for the events plane** (#583):
  `configs/router-events-storage.json5`. Events are durable in *transit* but the
  bus stores nothing, so the GUI's startup backfill GET (#536) returned nothing
  after a restart and a trap that fired while nobody was looking was gone. With
  this running, that GET is answered by the router.
  - It uses a plain `fs` volume, which contradicts RFC 09 §2's InfluxDB sketch
    on purpose: that guidance is about *per-key* history, and every event record
    owns a unique ULID key, so "latest per key" and "the whole log" are the same
    set. The config says so, and `router-verify` now proves it — two records
    under one subject must both survive.
  - The GUI's local redb cold store is **additive**, not superseded: records are
    immutable and ULID-identified, so the union needs no precedence rule.
  - The startup backfill drain is now bounded (`EVENT_BACKFILL_MAX`). Unbounded
    was harmless while nothing answered that GET; with a storage aligned it is
    answered by the entire stored log, and no narrower selector exists (ULIDs
    sort by time, but RFC 02 P6 forbids the sub-chunk wildcard that would let a
    consumer ask for a prefix).

### Changed

- **`introspect` can no longer ship lies** (#484). RFC 08 §6.1's MUST — every
  registered procedure is served by the build advertising it — is now checked at
  run time, immediately before the `alive` token. Debug builds panic; release
  builds warn. Every `declare_queryable` goes through
  `zensight_common::served::serve_queryable`, and a CI guard bans the raw call
  so the check cannot be bypassed by accident.
- **An origin you address is a type, not a string** (#485). `origin_rpc_key`
  takes a parsed `zenkey::RemoteOrigin`, so building an `@rpc` key aimed at your
  own host is a compile error rather than a timeout in one view. That bug
  shipped three times and was fixed by splitting the API by name — but both
  halves still took `&str`, so nothing stopped a fourth.
- **async-snmp 0.17** (#577). Upstream fixed the v3 engine wedge, so the
  client-rebuild workaround is gone in favour of `rediscover_engine()`.
- **`stream.rs` no longer documents a `tiers/set` command that never existed**
  (#513). The tier ladder is config-only; no build ever served that procedure
  and the registry declares none, so the type was documenting a bus nobody
  built.

### Changed — BREAKING

- **zblob 0.3 (wire v3).** v2 and v3 peers do not interoperate: every wire
  tag is re-spelled and the wire version moves to 3, so a mixed deployment
  fails closed rather than half-decoding — **sensors and frontend upgrade
  together** (again). Unlike the sha256→blake3 cut below, **chunk addresses
  do not change**: the GUI's redb chunk store and any router-hosted storages
  stay warm across the upgrade. Also picked up from 0.3: typed
  serve/query prefixes replace string hygiene (a wildcard fetch prefix is
  now unrepresentable, not just guarded), tier-2 materialization hardening
  (a hostile snapshot index can no longer delete pre-existing directories,
  apply setuid/setgid bits, or escape via a symlink chain), batched tier-2
  chunk fetches, and snapshot chunking derives its CDC min/max from the
  configured average (previously `avg == max` degenerated FastCDC into
  fixed-size chunking, and a `chunk_size` above 256 KiB failed validation).
- **zblob 0.2 (wire v2).** Artifact transfer moves to BLAKE3 + bao verified
  streaming, postcard control messages and chunk-range resume. v1 and v2
  peers deliberately do not interoperate — v2 renamed the reply keys so a
  mixed fleet fails closed instead of corrupting — so **sensors and frontend
  upgrade together**. Every reply is verified against the blob's content root
  *before* it touches disk, so a wrong or tampered slice is discarded rather
  than assembled and detected at the end.
  - `Delivery::Blob`'s manifest changes shape: `filename` is optional and
    advisory, and `chunk_count`/`hash_algo`/`hash` give way to `root` (the
    BLAKE3 bao root). The caller now names the destination *file*; the crate
    never joins a remote-supplied filename to a path.
  - The GUI's redb chunk store re-keys from `sha256/<hex>` to
    `blake3/<hex>`. Dedup is per-algorithm, so pre-0.11 chunks are inert
    rather than wrong — the store refills on the next fetch. It also gained
    the `hashes()`/`remove()` that the 0.2 `ContentStore` trait requires.


- **The SNMP event feed persists, filters and cross-links** (#578). Trap records
  land in a new redb `events` table keyed by their ULID — chronologically
  sorted, so "most recent N" is a bounded reverse walk and re-delivery is
  idempotent. No sampler, unlike logs: an event is already a rare, deliberate
  record. Facets + free-text search, and rows link to the device.
- **Logs history is backfilled from the sensors' durable stores** (#603).
  Opening Logs seeded only from the local redb cache and whatever the sensors'
  500-line hot rings still held — the authoritative unsampled store from #544
  was never asked, because a sensor reads it only when the query carries
  `from=`/`to=`/`after_uid=` and the GUI sent none on open.
- **Deep log-history pagination** (#601). The feed did a silent
  `truncate(100)`: an operator on a busy feed could not tell rows were withheld,
  nor reach them. The cap is now a window with a footer that says so, and a
  "load older" cursor walk against the history the sensor has served since #544.
- **Log export gains a format choice, and artifacts can carry a producer
  caveat** (#602). The export request hardcoded JSONL; the format is now picked
  next to the button. `ArtifactState::Ready` gained an optional `note` a
  producer sets via `ctx.note()`, so a bundle that had to truncate or skip
  something can say so instead of arriving silently incomplete.
- **A firing-alert headline tile on every protocol overview** (#582), plus a
  `firing_by_protocol` rollup beside `firing_by_source` and a protocol filter on
  the Alerts view.
- **SNMP subnet-discovery proposals on the fleet overview** (#579). The opt-in
  sweep from #541 published its report to `state/snmp/discovery` where only
  `zenctl` could see it. Proposals only — nothing auto-adds.


- **Every exported Prometheus/OTel series for the SNMP sensor is renamed**
  (#559, #647). The built-in MIB tables published raw MIB object names straight
  onto the telemetry key (`sysUpTime.0`, `ifInOctets`) — names the key chunk
  grammar forbids, so every debug poll cycle panicked in the metric guard and
  `refine_key` could not classify SNMP telemetry at all. All 49 built-in names
  now follow the lowercase, profile-style convention the shipped profiles
  already used.

  | | before | after |
  |---|---|---|
  | Prometheus | `zensight_snmp_sysUpTime_0` | `zensight_snmp_system_uptime` |
  | Prometheus | `zensight_snmp_ifInOctets_3` | `zensight_snmp_if_3_in_octets` |
  | OTel | `zensight.snmp.sysUpTime.0` | `zensight.snmp.system.uptime` |
  | OTel | `zensight.snmp.ifInOctets.3` | `zensight.snmp.if.3.in_octets` |

  **This hits stock deployments, not just exotic ones.** Profiles have been on
  by default since #531 and already used lowercase names — but before #559
  built-ins *won* over profiles (`add_profile_mappings` inserted with
  `.entry().or_insert()`), so for any OID both tables covered, the mixed-case
  built-in name is what got published.

  **Dashboards, recording rules and alerting rules built on the old names will
  stop matching** — silently, not with an error. The full 49-row table is in
  [`zensight-sensor-snmp/docs/reference.md`](zensight-sensor-snmp/docs/reference.md).
  Table columns also move the index into its own key chunk (`ifInOctets.3` →
  `if/3/in_octets`), so a per-interface series that was one flat name is now
  structured.

  **No compatibility aliases are published, deliberately.** SNMP is the
  highest-cardinality producer in the fleet (per-column × per-interface ×
  per-device); emitting both spellings would double that on the wire and in
  every scrape, permanently, to save a one-time dashboard edit. The GUI keeps
  *read-side* aliases so a fleet part-way through the upgrade still renders —
  those cost nothing on the wire and go away once no pre-0.11 sensor remains.

  **Unlike the logs rename in 0.10.0, `introspect` cannot tell you the old names
  are gone.** That one moved registry *subject paths*, leaving `deprecated.lock`
  entries a consumer can query. The SNMP registry subject is the rest-var
  `{device}/{metric...}` and the rename happened *inside* it, so no subject was
  retired and there is nothing in the ledger to find. This entry and the crate
  reference are the only record.

  The *keyspace* change is not breaking: consumers subscribe by class wildcard
  (`v1/*/telemetry/**`), so no subscription changes.

  Custom `oid_names` violating the grammar are no longer a panic — they are
  escaped losslessly at the publish boundary and warned about at startup — so a
  stale config now yields a **third** spelling matching neither scheme
  (`system/sysUpTime` publishes as `system/sys_x55_p_x54_ime`).
  `docker/configs/snmp.json5` was shipping exactly that, and is fixed here.

- **The deprecated JSON pseudo-MIB support is removed** (#580). `snmp.mib.files`
  is now a hard startup error pointing at `snmp.mib.dirs`, rather than a
  warning. Deprecated in #532 and warned through 0.10.x.

- **zblob 0.3 (wire v3).** v2 and v3 peers do not interoperate: every wire
  tag is re-spelled and the wire version moves to 3, so a mixed deployment
  fails closed rather than half-decoding — **sensors and frontend upgrade
  together** (again). Unlike the sha256→blake3 cut below, **chunk addresses
  do not change**: the GUI's redb chunk store and any router-hosted storages
  stay warm across the upgrade. Also picked up from 0.3: typed
  serve/query prefixes replace string hygiene (a wildcard fetch prefix is
  now unrepresentable, not just guarded), tier-2 materialization hardening
  (a hostile snapshot index can no longer delete pre-existing directories,
  apply setuid/setgid bits, or escape via a symlink chain), batched tier-2
  chunk fetches, and snapshot chunking derives its CDC min/max from the
  configured average (previously `avg == max` degenerated FastCDC into
  fixed-size chunking, and a `chunk_size` above 256 KiB failed validation).
- **zblob 0.2 (wire v2).** Artifact transfer moves to BLAKE3 + bao verified
  streaming, postcard control messages and chunk-range resume. v1 and v2
  peers deliberately do not interoperate — v2 renamed the reply keys so a
  mixed fleet fails closed instead of corrupting — so **sensors and frontend
  upgrade together**. Every reply is verified against the blob's content root
  *before* it touches disk, so a wrong or tampered slice is discarded rather
  than assembled and detected at the end.
  - `Delivery::Blob`'s manifest changes shape: `filename` is optional and
    advisory, and `chunk_count`/`hash_algo`/`hash` give way to `root` (the
    BLAKE3 bao root). The caller now names the destination *file*; the crate
    never joins a remote-supplied filename to a path.
  - The GUI's redb chunk store re-keys from `sha256/<hex>` to
    `blake3/<hex>`. Dedup is per-algorithm, so pre-0.11 chunks are inert
    rather than wrong — the store refills on the next fetch. It also gained
    the `hashes()`/`remove()` that the 0.2 `ContentStore` trait requires.

### Fixed

- **The eBPF features job had never once got past installing its linker**
  (#674). `cargo install bpf-linker --locked` builds an LLVM frontend against a
  *system* LLVM, and the runner image ships none: the only run this workflow has
  ever had spent 70 seconds compiling before dying on "could not find
  llvm-config in directories specified by environment variable `PATH`".
  Upstream's own build script says as much before it fails — a source build "is
  NOT recommended for regular users" — and publishes a statically linked release
  binary for the purpose, which is what the job now fetches: pinned to v0.11.0,
  27 MB over the wire, and installed to `/usr/local/bin` rather than
  `$CARGO_HOME/bin` so a 104 MB executable stays out of the Rust cache, whose
  key knows nothing about the linker's version and would have restored a stale
  copy on every bump. The workflow also gains a path-filtered `pull_request`
  trigger, because the deeper problem was that nothing but a manual dispatch
  could ever run this file — which is how a job that had never completed landed
  on master. The 04:17 UTC cron is untouched: it had not been failing nightly,
  it had not yet run at all (Forgejo schedules only from the default branch, and
  the workflow arrived there the same day the issue was written).

- **netlink's connect latency measures the handshake, not the SYN it sent**
  (#114). The probe sat on a kretprobe on `tcp_v4_connect()`, which builds and
  sends the SYN and returns — the handshake wait happens afterwards in
  `inet_stream_connect()`, and for a non-blocking socket there is no wait at
  all. Loaded on a real host against a 200 ms netem RTT, it reported **16–64 µs**:
  a ~6000x understatement, and one that never looks empty — it looks like a
  suspiciously fast network. It now stamps at `CLOSE → SYN_SENT` and measures at
  `→ ESTABLISHED`, both edges of the `inet_sock_set_state` tracepoint that was
  already attached for tcplife, so it costs no new offsets and **deletes four
  kprobes** (and with them the failure mode where a kernel without a
  `tcp_v6_connect` symbol aborted the whole load). Verified at two independent
  delays: 100 ms RTT → bucket 18 (131–262 ms), 5 ms RTT → bucket 14 (8–16 ms).
  - Refused connects no longer enter the histogram. They go `SYN_SENT → CLOSE`
    and never reach the measurement point, so they are excluded by construction
    rather than by checking a return value the kretprobe never looked at — 20
    refused connects moved the total by 0.
  - **Connection ownership is now the process that opened the socket.**
    `pid`/`comm` were read at ESTABLISHED and CLOSE, which are frequently
    softirq context: a 60-connection `curl` loop was attributed to `curl` in
    only 59 of 91 records, the rest going to `bash`, `python3`, `claude` and
    twice to `ksoftirqd/1`. The identity is captured at `CLOSE → SYN_SENT` —
    inside `connect(2)`, in the caller's own context — and replayed at both
    later edges. 110/110 after the fix.
  - The tracepoint is shared with DCCP and SCTP, so a protocol guard now drops
    non-TCP transitions before their state numbers can be read as TCP ones.

- **An eBPF load failure now says why** (#168). Both loaders logged
  `tracing::warn!(error = %e, …)`, and `Display` on an `anyhow::Error` prints
  only the outermost context — `"load eBPF bytecode"` — discarding the aya error
  underneath it, including the verifier log. A rejected program was
  indistinguishable from an `EPERM`, which is the worst possible property for a
  subsystem whose entire remaining work item is on-host validation. Both now log
  the full chain, and it paid for itself immediately: the first unprivileged run
  named its own cause (`attach sched/sched_wakeup: perf_event_open_trace_point
  failed: Permission denied`) instead of shrugging.

- **The SNMP e2e harness had a 500 ms cliff under load** (#668).
  `collect_points` waited for *silence*, not for the points it wanted: a cycle
  whose first sample took longer than the 500 ms idle gap returned an empty map,
  and the caller indexed it, so the failure read `no entry found for key` with
  nothing pointing at a timeout. It now waits up to 5 s for the first sample and
  keeps the short idle gap between samples — the two are different quantities,
  and only the first moves under load. The callers that assert a cycle published
  *nothing* use a new `collect_quiet`, which keeps the old semantics, because
  waiting longer for a point that must never come is only slower.

- **A host without the resource made `introspect` lie again** (#666, #648
  follow-up). `zensight-sensor-systemd`'s `@rpc` channel connected to the system
  D-Bus *before* declaring anything and returned on failure, so a host with no
  reachable bus — a container started without the
  `/run/dbus/system_bus_socket` mount is the everyday case — advertised
  `units`, `failed`, `unit`, `unit/file`, `timers`, `events` and `cgroups` and
  answered none of them. #648 closed the build-feature, config-flag and
  capability doors; this is the same class through a fourth, resource
  acquisition order. `zensight-sensor-netlink` had the identical shape ahead of
  ten procedures, reachable from a sandbox that restricts `AF_NETLINK`. Both now
  declare first and answer `error/systemd/no-system-bus` /
  `error/netlink/no-route-socket` — neither `gated` (nothing is switched off)
  nor `unsupported` (the build has the capability), because a caller that
  cannot tell those apart is back to the silence the check exists to prevent.

- **`inform_v2c_is_acknowledged` asserted nothing about acknowledgement**
  (#663). `send_inform` swallows per-sink failures and returns `Ok(())`
  unconditionally, so the test's `.expect("inform must be acknowledged")` could
  not fail — an inform that timed out and retransmitted itself to death logged
  a warning and passed. The test now uses `send_inform_detailed` and asserts
  `outcome.failures()` is empty, the form #650's restart e2e already used. Test
  only; no shipped behaviour changes.

- **Trap alerts were never published when `snmp.alerts.for_secs > 0`.** The
  trap path used the reporter's default debounce, which only publishes once a
  *second* observation arrives after the window — but a trap is a single
  observation, so the alert was entered as active and never sent. It now passes
  an explicit zero debounce: a one-shot event has no "sustained for" semantics.
  Default `for_secs` is 0, so stock deployments were unaffected; anyone who set
  it lost trap alerting entirely, silently.

- **The SNMPv3 trap receiver minted a fresh engine identity on every start**
  (#650). When `trap_listener.users` is configured this sensor is an
  authoritative SNMP engine — informs are authenticated against *its*
  `snmpEngineID` and `(boots, time)` window, and it signs the automatic
  acknowledgement with them — so RFC 3414 §2.2 requires a stable id and a
  monotonic, persisted `snmpEngineBoots`. It had neither.
  The cost was worse than the re-handshake the code comment claimed: a sender
  that had already discovered this engine had its informs **dropped outright**
  (localized to an authoritative engine the receiver no longer had), with no
  acknowledgement, until it rediscovered. `(engine_id, boots)` now persist to
  `trap_listener.engine_state_path` — defaulting to the systemd
  `STATE_DIRECTORY` / XDG state location — written atomically, and boots
  increments on each start.
  - **The shipped systemd unit gains `StateDirectory=zensight-snmp`.** Under
    `ProtectSystem=strict` that is the only writable path, so a unit without it
    could not persist anything.
  - A location that resolves but **cannot be written refuses v3 receiving**
    (v1/v2c listening continues) rather than silently downgrading: an operator
    who asked for durability and did not get it should hear it from the log, not
    from a sender. A host that resolves *no* durable location at all keeps the
    old ephemeral identity with a warning — it never asked for durability, and
    refusing would turn an upgrade into an outage.
  - A stored `boots` latched at the RFC maximum mints a **new** engine id;
    restarting into a latched engine rejects all authenticated inbound.


- **Tier-2 artifact fetches were trust-on-first-use** (RFC 07 §2.1/§2.3).
  `Delivery::Tree` named the snapshot by a caller-minted ULID, and the root
  hash it *did* carry (`TreeSummary::root_hash_hex`) was documented as the
  "integrity root" and checked by nobody — so the consumer asked for a name
  and trusted whatever index answered. Snapshot indexes are now
  content-addressed: the key **is** the root, and the fetch uses
  `DownloadRequest::by_root`, which cannot express trust-on-first-use.
  `root_hash_hex` is removed rather than left unused, so nothing can mistake
  it for a verified value again. Tier-1 fetches are likewise pinned to
  `manifest.root`.
- **Capture downloads used a wildcard-origin bulk GET** (RFC 07 §3). The GUI
  did not know which host held a capture, so it fetched under
  `v1/*/@blob/artifact` — every matching holder ships the full payload and
  Zenoh cannot cancel remote replies in flight, so the cost was bounded only
  by artifact ids happening to be unique ULIDs rather than by the protocol.
  The origin was already known one hop upstream and simply discarded:
  `CaptureRecord` now carries `artifact_prefix` (the concrete origin) and
  `artifact_root`, both `#[serde(default)]`. `fleet_blob_prefix()` is gone,
  with a `keyexpr.rs` guard test asserting no builder can hand out that shape
  again. A record without an origin shows "sensor too old" instead of a
  Download button — such a sensor is pre-wire-v2 and could not answer this
  build anyway.
- Artifact blob/tree servers are declared before the channel starts answering
  requests (zblob 0.2's `spawn()`), closing a window where a request could
  race ahead of the server meant to serve its bytes.

## [0.10.1] - 2026-07-27

### Fixed

- The `production` profile also silences the two default-ON alert sources
  the 0.10.0 fleet still carried tombstones for: netring's TRW port-scan
  detector (`port_scan: false` — VPN/monitoring traffic is scan-shaped
  enough to false-fire) and netlink's `demo-expected-service` sentinel
  expectation (designed to always fire so `just run` can demo the alert
  pipeline; deleted in production — the `no-telnet` forbid rule stays, it
  cannot false-fire on a clean host). demo-max is unchanged.

## [0.10.0] - 2026-07-27

Alert-noise release: the log novelty detector is gone, and the sensors
container now defaults to a quiet **production** profile with the
anomaly/security detector suite off. Field experience from the first fleet
deployment (0.9.0): per-template "new log pattern" alerts and the NDR
detector suite were near-pure false positives on a normal server fleet.

### Changed — BREAKING

- **Log novelty / rate-spike detection removed** (#103 retired): the
  `log-novelty` ("new log pattern: …") and `log-rate-spike` alerts, the
  `syslog.novelty` config block and the tracker are deleted. Template mining
  (#102) itself stays — `template_id`/`template` labels and
  `by_template/*` rollups are unaffected. **Migration:** the strict config
  loader (#547) rejects unknown keys, so a config still carrying a
  `novelty:` block fails to load — delete the block when upgrading.

- **The sensors container defaults to the new `production` profile.**
  `gen-configs.sh` grows `--profile demo-max|production`;
  `docker/entrypoint-sensors.sh` defaults to `production`
  (`ZENSIGHT_PROFILE=demo-max` restores the previous all-on behavior, and
  `just configure`/`just run` still use demo-max). In production the netring
  detector suite (beaconing/RITA ×2, DNS tunnelling, newly-observed domains,
  DGA, data-exfil, encrypted-DNS bypass, connection floods), the log
  error-budget burn alerts and the tmpfs-backed durable log store stay at
  their shipped defaults (off). Telemetry stays rich: L7 collectors, sysinfo
  opt-in collectors + thermal alert, systemd ops alerts and the on-demand
  debug reports remain on.

## [0.9.0] - 2026-07-26

SNMP and logs grow from pollers into full subsystems (typed models, durable
events, alerting, GUI views), the bus gains an append-only **events class** and
**TLS/mTLS transport to a zenoh router**, and the repo slims down: `zblob` and
`zenkey` graduate to their own repositories and come back as crates.io
dependencies. CI moved from GitHub Actions to Forgejo Actions; deb/rpm
packaging is retired in favor of container images and a binary tarball.

### Changed — BREAKING

- **The repo splits: `zblob` and `zenkey` graduate to their own repositories**
  (#518). The in-tree `zenoh-blob/` and `zensight-keyspace/` crates are gone;
  zensight consumes `zblob` and `zenkey`/`zenkey-build` from crates.io. The
  keyspace registry TOMLs stay application-owned and move to
  `zensight-common/registry/`, compiled by `zenkey-build` from the build
  script. **Migration:** patches against the old in-tree crates must target
  the new repos; local cross-repo work needs a temporary `[patch.crates-io]`
  path override in the consumer's root manifest.

- **zenkey 0.3 migration** (from the in-tree 0.1 line): typed `Key`/origin
  minting, codegen v2, RFC v1.5 — every producer now serves
  `@rpc/<producer>/describe` (RFC 08 §7 `SchemaSet`) next to `introspect`,
  and the repo carries a build-lint-enforced `registry/types.toml` type table.

- **SNMP poller migrated to `async-snmp`** (#526): persistent per-device
  sessions, GETBULK, retry/backoff in the library, no C dependencies. The
  poller config surface changes (session/bulk tuning replaces the old
  per-request knobs) — re-check `configs/snmp.json5` against your deployment.

- **SNMP counter semantics** (#527): counters now publish **derived rates**
  with wrap/reset detection, typed values and units, instead of raw
  monotonically-increasing samples. Consumers (dashboards, exporter scrapes,
  alert thresholds) that expected raw counters must be re-pointed at the new
  rate series.

- **SNMP typed interface model** (#529): per-device joined ifTable/ifXTable
  **state documents** replace the flat per-OID telemetry for interfaces; the
  GUI device view (#530) and fleet overview (#533) read the typed doc.

- **SNMP trap pipeline** (#535): v3 traps and informs (with acks), MIB
  translation, and **durable events** on the events class, with alert
  mapping — trap handling that previously surfaced as ad-hoc telemetry now
  lands as `events/snmp/…` records.

- **`zenoh.namespace` no longer defaults to `zensight` — the empty base is the
  legal default** (RFC 03 §1.1 as amended). The base names a *deployment*, not
  the software, so the software ships no default: unset/empty now means **no
  session namespace is set** (Zenoh's own default) and the deployment's full
  wire keys start at `v1/…`. Setting a base (`zenoh.namespace` /
  `ZENSIGHT_ZENOH_NAMESPACE`) is the opt-in isolation knob for running several
  deployments on one Zenoh infrastructure. **Migration:** a deployment that
  relied on the old implicit `zensight` default must now set it explicitly
  (`zenoh: { namespace: "zensight" }` on every participant, or
  `ZENSIGHT_ZENOH_NAMESPACE=zensight`) — otherwise its wire moves from
  `zensight/v1/…` to `v1/…` on upgrade, and mixed old/new fleets cannot see
  each other. Router storage example configs (`configs/router-*.json5`) and
  the `v1_probe` example now assume the base-less wire; prefix their selectors
  with your base if you set one. `zensight_common::DEFAULT_BASE` is renamed to
  `CONVENTIONAL_BASE` (diagnostics-only — no longer a default).

### Added

- **Zenoh TLS/mTLS client support**: an optional `zenoh.tls` config block
  (`root_ca_certificate`, `connect_certificate`, `connect_private_key`,
  `enable_mtls` — names mirror Zenoh's `transport/link/tls` keys) on every
  sensor, exporter, correlator and the GUI, overridable via
  `ZENSIGHT_ZENOH_TLS_{CA,CERT,KEY,MTLS}` for launchers without an editable
  config file (the flatpak GUI, the sensors container — whose entrypoint now
  fails fast when a set TLS path is not mounted). Connect with a
  `tls/<router>:7447` endpoint; see `docs/DEPLOYMENT.md` §TLS.

- **Events class instantiated** (#534): `EventRecord`/`EventPublisher` +
  `QosClass::Event` — the append-only third class next to telemetry and
  state; first producers are the SNMP trap pipeline and the logs sensor.

- **Logs epic** (#542, #543–#558): TLS syslog listener (RFC 5425, rustls,
  mTLS, cert reload) (#550) · rotation-aware, position-persisted file tailing
  (#549) · durable redb log history with retention and paginated query
  (#544) · declarative log-sentinel pattern→alert rules (#543) · server-side
  regex/field search (#553) · observer evidence for remote syslog senders
  (#552) · log bundle / filtered-feed export artifacts (#555, #607, #608) ·
  one `LogSeverity` model across common+GUI (#557) · GUI regex filter,
  global-search routing, time-range picker, context-rich alert rows
  (#554, #556, #558, #609) · ingest robustness — RFC 3164 year/timezone
  inference, channel caps, repeat collapse, multiline re-parse
  (#545–#547, #584) · in-process e2e harness (#548).

- **SNMP epic remainder** (#526–#541): threshold alert engine (#528) ·
  sysObjectID-matched device profiles (#531) · real SMI MIB support — vendor
  MIB dirs, enum decode, units, trap translation (#532) · identity evidence
  for polled devices (#537) · credential hygiene — secret indirection, named
  sets, scrubbing audit (#538) · resilience — per-device backoff, circuit
  breaker, poll jitter (#539) · subnet auto-discovery (#541) · GUI device
  view, fleet overview, trap/event feed (#530, #533, #536) · in-process e2e
  harness with sim agent and v3 matrix (#540).

- Liveliness late-join via zenoh history (#520) and RFC 08 §7 payload
  self-description on samples; GUI renders fan speed, battery and RAPL power
  (#516); `zenctl` becomes app-agnostic and lives in the zenkey repo
  (tcgui#45); `just run` demos the full surface (hwmon, detector suite,
  sysinfo eBPF).

- **Release artifacts**: a `zensight-correlator` container image (the one
  mandatory-per-deployment piece was previously not shipped) and a
  `zensight-<ver>-linux-amd64.tar.gz` with all 12 binaries, the hardened
  systemd units and example configs, for native installs.

### Infrastructure

- **CI moved to Forgejo Actions** (`.forgejo/workflows/`); the GitHub
  workflows are retired and GitHub is a passive mirror. **deb/rpm packaging
  is discontinued** — container images (now at
  `git.marcpardo.eu/marcpardo/*`) and the binary tarball replace it.
- Release images are built inside `rust:1.97-bookworm` so binaries link
  against the runtime base's glibc, smoke-tested in-image before push;
  `workflow_dispatch` dry-runs the whole pipeline without publishing;
  tags now run the full test suite.
- rustc pinned to **1.97** repo-wide (root `rust-toolchain.toml`, CI,
  image builds, flatpak SDK 25.08) in lockstep with the build cluster;
  sccache (garage S3) caches compilation across all repos.

## [0.8.0] - 2026-07-16

The v1 keyspace. Every key on the bus moves to the ratified keyspace-v2 grammar
(`<base>/v1/<origin>/<class>/<producer>/<subject…>`), the control plane becomes
`@rpc`, alerts become last-writer-wins state documents, the correlator becomes
`@catalog`, and parallax gains demand-driven tiered simulcast. This is the
largest breaking surface in the project's history — **there is no compatibility
shim, and a 0.7.0 deployment will not interoperate with a 0.8.0 one.**

Upgrading from 0.7.0? Read the migration table in
[`docs/plans/keyspace-v2/RETROSPECTIVE.md`](docs/plans/keyspace-v2/RETROSPECTIVE.md)
(§2, "The keys themselves" / "What was *deleted*") — it maps every old key to its
v1 form. The normative spec is [`docs/rfcs/keyspace-v2/`](docs/rfcs/keyspace-v2/00-index.md);
the deployed-profile summary is [`docs/KEYSPACE.md`](docs/KEYSPACE.md).

### Changed — BREAKING

- **Every key on the bus moves to the v1 grammar** (epic #453, #455–#465). Keys are
  now `<base>/v1/<origin>/<class>/<producer>/<subject…>` with classes
  `telemetry`/`state`/`events`, verbatim planes `@rpc`/`@media`/`@blob`, and the
  `@catalog` identity service. Telemetry that was
  `zensight/sysinfo/toolbx/cpu/usage` is now
  `zensight/v1/h-9706b31ddad3/telemetry/sysinfo/cpu/usage`. There is **no legacy
  shim** — `zensight-sensor-core/tests/cutover_e2e.rs` subscribes to the entire
  legacy bus (`zensight/**`) and asserts it stays silent. The typed builders live in
  `zensight-keyspace`; never `format!` a key.

- **`key_prefix` is retired from every sensor config** (#465). Producers are *named*
  (`SensorConfig::producer()`), never prefixed. **This is the breaking config change,
  and it fails quietly**: nothing in the workspace sets `deny_unknown_fields`, so a
  `key_prefix:` line left in a 0.7.0 config is **silently ignored** rather than
  rejected. Delete it from all sensor configs. The `SensorInfo.key_prefix` wire field
  is likewise renamed to `producer`.

- **The base is the session namespace, not a key chunk** (#466). `zensight/` is no
  longer spelled in keys — sessions are opened namespaced and keys are declared
  base-relative. Same bytes on the wire; an unnamespaced client must add the prefix
  itself. New optional `namespace` knob (`ZENSIGHT_ZENOH_NAMESPACE`, default
  `zensight`); an empty or wildcard namespace is refused.

- **The version chunk is plain `v1`, not verbatim `@v1`** (#482). Any consumer
  literal containing `@v1` breaks. This one *fixes* a silent bug: `**` never crosses
  an `@`-chunk, so zenoh-ext's `@adv` publisher-detection tokens were unparseable and
  **late-publisher detection had never worked**.

- **Commands become `@rpc` queryables** (#460). The pub/sub command plane is gone:
  `put zensight/<p>/@/command` → GET `…/@rpc/<producer>/<topic>` (read) or
  `…/@rpc/<producer>/<topic>/set` (write). Errors now ride `reply_err` instead of a
  status document. The `@/status` document plane is retired (the health doc absorbed
  the running flag).

- **Alerts become LWW state documents** (#461). The shared `@/alerts` blob is gone —
  one document per alert at `state/<producer>/alert/<16hex>`, keyed FNV-1a 64 over
  rule + sorted labels. The source is no longer hashed and the CamelCase rule prefix
  is gone, so **alert keys differ from 0.7.0**. Seeding is now a storage-shaped GET.

- **The correlator becomes `@catalog`** (#462). Entities publish at
  `@catalog/state/entity/<id>`, with `alias/<old-id>` and `pdns/<ip-slug>`. Ownership
  is a liveliness claim plus lexical election — losers exit rather than double-serve.

- **Telemetry trees become real registry subjects** (#468, #479). sysinfo is 113
  declared subjects instead of one catch-all, and five more trees followed; the
  registry stops dropping the type column, and parallax's declared-but-nonexistent
  payload type is gone. `@rpc/<producer>/introspect` now describes exactly what the
  build serves.

- **parallax: `<profile>` → `<tier>`, and the wildcard licence is revoked** (#494).
  Video rides `@media/parallax/<stream>/video/<codec>/<tier>` where `<tier>` is a
  named bandwidth rung (low/medium/high). **Viewers must subscribe to an exact tier**
  — `…/video/h264/*` is no longer permitted, because each tier is an independent
  encoder pipeline and a wildcard would pull all of them. `StreamControl`'s
  `OpenStream`/`CloseStream` carry `{codec, tier}`; `StreamStatus` is per-tier.
  Per-viewer quality is expressed by *which tier you subscribe to*, not by a command.

- **GUI: devices are keyed on the publishing origin, not the hostname** (#474, #483).
  `DeviceId` becomes `{protocol, origin, source}`. This fixes silent misrouting of
  `@rpc` drill-downs when two hosts share a hostname, and the empty-map fallback in
  the first few seconds after connect.

- **The exported Prometheus/OTel series for the logs sensor are renamed** (#470).
  The logs sensor was the only producer that prefixed its *metric names* with its
  own producer name, so every key carried the chunk twice
  (`…/telemetry/logs/logs/errors_total`) — and because both exporters derive the
  series name from `point.metric` rather than from the key, that doubling was
  visible in every dashboard:

  | | before | after |
  |---|---|---|
  | Prometheus | `zensight_logs_logs_errors_total` | `zensight_logs_errors_total` |
  | OTel | `zensight.logs.logs.errors_total` | `zensight.logs.errors_total` |

  **Dashboards, alert rules and recording rules built on the old names will stop
  matching and must be updated.** All 18 logs metric families are affected
  (`errors_total`, `warnings_total`, `units_in_failure`, `ingest/*`,
  `by_severity/*`, `by_unit/*`, `by_template/*`, `journald/*`).

  The *keyspace* change is **not** breaking: every consumer subscribes by class
  wildcard (`v1/*/telemetry/**`), so no subscription needs to change. The retired
  subject paths are recorded in `zensight-keyspace/registry/deprecated.lock` and
  may never be re-used (RFC 08 §3), so `introspect` can tell a consumer that a key
  it remembers is *gone* rather than merely absent.

### Added

- **`zensight-sensor-parallax` — live video onto the media plane** (epics #402 and
  #494). A new sensor built on the `parallax` pipeline engine advertises V4L2
  cameras, RTSP cameras, and synthetic test patterns as a stream catalogue
  (GET `@rpc/parallax/streams` → `Vec<StreamDescriptor>`), opens and closes encode
  pipelines via `@rpc/parallax/stream/set`, and publishes opaque H.264 access units
  (`@media/parallax/<stream>/video/<codec>/<tier>`) and low-fps JPEG previews
  (`@media/parallax/<stream>/preview/jpeg`) with a typed CBOR `FrameMeta` attachment
  per frame (#403). Streams are refcounted per open, torn down on close or after an
  idle window without viewers, and force a keyframe the instant a subscriber appears
  (#404–#406). Per-stream stats (`<stream>/stats/{fps,kbps,drops,viewers,encode_ms}`),
  per-stream device health, and auto-resolving alert rules (`camera_disappeared`,
  `rtsp_connect_failed`, `encoder_overrun`) ride the normal channels (#407).

  **Packaged? No — parallax is source-only in 0.8.0.** It is not in the deb/rpm set,
  not in the `zensight-sensors` container image, and has no systemd unit. Build it
  with `cargo build --release -p zensight-sensor-parallax` (it compiles openh264 from
  C++ source). Packaging it is tracked separately.

- **parallax: demand-driven tiered simulcast** (#494, on parallax-pipeline 0.3.0).
  A stream offers a ladder of named tiers (low/medium/high, each a `TierSpec` of
  height/fps/bitrate, capped at the source's native resolution); each tier that a
  viewer actually subscribes to gets its **own independent encoder pipeline**, started
  on first subscriber and stopped on last. Bitrate is adjustable live without
  restarting the pipeline. The GUI subscribes to an exact tier, offers a per-tier
  Live button with an annotated tier picker, and shows a bandwidth readout
  (#502, #503).

- **GUI: parallax stream catalogue + live JPEG preview tiles** (#408). The
  parallax device view fetches the catalogue on open and renders abortable
  live preview tiles (exact-key media subscriber, latest-frame-wins, CBOR
  `FrameMeta`, JPEG decode off the UI thread) with seq/fps captions; every
  way of leaving the view tears the tiles down and closes the streams.

- **GUI: opt-in H.264 live view** behind the new `zensight` `h264` cargo
  feature (#409; default OFF — openh264 is a C++ build from source). Decodes
  the selected tier keyframe-gated, rebuilds the decoder and requests a fresh
  IDR on sequence discontinuities; default builds show a build hint instead.

- **`zenctl` — a bus explorer for the v1 keyspace** (#479, RFC 08 §6).
  `topic list/info/echo`, `node list`, `service list/call`, and `doctor`, all driven
  by the registry rather than by hand-written key strings.

- **`@catalog`: operator link/unlink** (#473, #486). An operator can assert the
  identity that evidence cannot infer — linking two origins into one `HostEntity`, or
  splitting one that was fused wrongly. Assertions outrank inferred evidence and
  survive restarts.

- **GUI: the Fleet view** (#469). `introspect` finally has a caller: the fleet is
  rendered from what each build declares it serves, and a dead sensor is now reported
  as offline rather than merely `silent` (the alive set is gated on liveliness).

### Fixed

- **The router storage configs are verified against a real `zenohd`** (#471). The five
  storages across `configs/router-{blob,evidence,pdns-influxdb}-storage.json5` were
  re-expressed as v1 selectors during the cutover with no test covering them; they now
  have one (`#[ignore]`d — CI has no `zenohd`; run `just router-verify`).

- **The logs sensor no longer doubles its producer chunk** (#470) — see the breaking
  note above for the exported series rename.

- **The container image is rebuilt and CI-built** (#472). `docker/Dockerfile.sensors`
  had rotted since before the v1 cutover; CI now builds it on every push and
  `docker-compose.yml` matches it. **Not yet verified on a real podman host** —
  #472 stays open until `scripts/image-verify.sh` runs green.

### Dependencies

- **Bumped `nlink` 0.24 → 0.25** (netlink and netring sensors). 0.25 is largely
  internal correctness fixes; the only breaking surface here is the sockdiag
  `MemInfo` rework — `sndbuf`/`rcvbuf` are now `Option<u32>` and only populate
  when the filter requests `INET_DIAG_SKMEMINFO` (`with_sk_mem_info()`), which
  the socket collector/drill-down now do.
- **BREAKING (metric-value corrections)**: the bump above fixes several
  netlink metrics that were emitting wrong values under 0.24:
  - socket `snd_buf_total` / `rcv_buf_total` (and the per-socket `snd_buf` /
    `rcv_buf` in the `@rpc/netlink/sockets` drill-down) were **silently 0** —
    the code read the SKMEMINFO buffer sizes without requesting them. They now
    report real kernel buffer sizes.
  - ethtool link **speed/duplex** now populate (an enum-id misalignment made
    speed read `None` and duplex read garbage in 0.24).

## [0.7.0] - 2026-07-08

Identity & evidence release. Sensors now self-report a stable host identity and
republish observed hosts/names; a new `zensight-correlator` service fuses that
evidence into one `HostEntity` per physical host, and the GUI groups every
per-protocol facet under a single host card. This release also lands the Zenoh
low-bandwidth efficiency work (CBOR default, reliable alert/command traffic,
detail-on-request keyspaces, a media plane), a unified on-demand artifact
channel, container/cloud identity, durable storage tiers, new export paths, a
fully redesigned topology view (epic #395), and first-class multi-machine
deployment (host-scoped state keys, a sensors-only container image, and
`docs/DEPLOYMENT.md`). It carries a batch of deliberate breaking changes — see
**Changed (BREAKING)** and the per-entry mixed-version notes; upgrade sensors
and frontend together.

### Changed (BREAKING)

- **Per-sensor state keys are now host-scoped:
  `zensight/<protocol>/<source>/@/{health,errors,status,alive,devices/**}`.**
  Previously these lived at `zensight/<protocol>/@/…` with no `<source>`
  segment, so N machines running the same sensor overwrote each other's
  health/errors/status (last-writer-wins) and shared one liveliness token —
  the GUI showed one flapping card per protocol instead of one card per host.
  Sensors publish **only** the new shape; the GUI and correlator consume both
  shapes for one release (mixed-fleet rolling upgrade; legacy ingestion drops
  in 0.9). Third-party consumers of the old keys must move to
  `zensight/*/*/@/…` wildcards. The protocol-scoped channels
  (`@/alerts/*`, `@/commands/*`, `@/query/*`, `@/artifact/*`) are unchanged —
  their sharing is deliberate (fan-in queries, alert keys hash `source` in);
  see `docs/KEYSPACE.md` §3. `HealthSnapshot` gains an optional `source`
  field; `SensorRunner::new`/`new_with_args` take the instance source at
  construction (and `with_identity`/`with_artifacts` lost their now-redundant
  `source` parameters); `KeyExprBuilder::status_key()` takes the source.
  The GUI Sensors view now renders one card per instance (`sysinfo @ hostA`),
  and its artifact downloads set `ArtifactRequest.opts.target_source` from the
  card so only that host produces the artifact (aggregated views keep the
  fan-out).

- **netring NDR detectors migrated onto flowscope 0.22's `DetectorRegistry` +
  netring 0.29's `aggregate()`/`red()` (#369).** The hand-rolled
  `pattern_detector!` blocks (port-scan, CV/RITA beacon, connection-flood, DGA,
  DNS-tunnel, newly-observed-domain, data-exfil) are gone; the stock flowscope
  detectors now run in one `DetectorRegistry<FlowKey>` driven by netring's own
  flow + DNS stream. Runtime detector tuning (`@/commands/detectors`: allowlist /
  mute / per-detector threshold, #121/#328) is preserved by a `Tuned<D>`
  decorator that post-filters each stock anomaly against the live config. Three
  wire contracts changed:

  | Contract | Before | After |
  |----------|--------|-------|
  | Flow-lifetime telemetry | `flow/duration_p50_ms`, `flow/duration_p95_ms` | `flow/red/{rate,error_ratio,p50_ms,p95_ms,p99_ms}` (netring `red()`) |
  | `@/query/talkers` | `TalkerRecord{dst,bytes,packets,flows,names}` (per-dest cumulative) | `TalkerRecord{src,bytes_per_sec,names}` (per-source rolling 60 s rate) |
  | `@/query/matrix` | `MatrixRecord{src,dst,bytes,packets,flows}` (cumulative) | `MatrixRecord{src,dst,bytes_per_sec,names}` (rolling 60 s rate) |
  | Anomaly slug | `RitaBeacon` | `BeaconRita` (flowscope upstream) |
  | Anomaly slug | `DataExfiltration` | `DataExfil` (flowscope upstream) |

  Talkers/matrix now rank by rolling **bytes/sec** (netring `aggregate()`) rather
  than cumulative volume, and talkers are keyed by **source** IP; connection-flood
  is now source-keyed (stock detector) rather than `(dst,port)`-keyed. The
  `talkers?top=N` / `matrix?top=N` / `@/alerts` query keys themselves are
  unchanged. A pre-#369 GUI mis-reads a post-#369 sensor's talker/matrix replies
  and the renamed slugs — upgrade sensor + frontend together. See
  `docs/KEYSPACE.md` for the full contract.

### Changed

- **BREAKING — per-line log events moved off the streamed bus (#358).** The logs
  sensor no longer publishes each log line as
  `zensight/logs/<host>/events/<uid>` telemetry; lines land in a bounded
  in-memory ring (config `events_ring_capacity`, default 10 000) served on
  demand from a new `zensight/logs/@/query/events` queryable
  (`Vec<LogRecord>`, newest first; selectors `since=` inclusive / `max=` /
  `host=`). On a constrained link the per-line stream could dominate the
  telemetry bus — this brings logs in line with the "high-cardinality detail is
  served on request, never streamed" keyspace principle that flows/sockets/
  processes already follow. The low-rate rollups (`logs/by_severity/*`,
  `logs/by_unit/*`, …) stay streamed for charts/alerts. The GUI seeds its Logs
  view from the queryable on open and refreshes it on a slow (5 s) tick while a
  logs surface is visible, persisting fetched lines to the local store for
  search-back; it still ingests the old streamed shape from pre-#358 sensors.
  Mixed-version note: a pre-#358 GUI shows no log lines from a post-#358 logs
  sensor (upgrade both together).

### Added

- **Topology view redesigned (epic #395; design report
  `docs/TOPOLOGY-REDESIGN.md`).** The map is now a typed, directed,
  rate-weighted graph derived from data already on the bus: flow edges carry
  live bytes/sec from the netring traffic matrix (arrowheads only where a
  direction was observed), netlink neighbor tables draw dotted L2 adjacency,
  and each host links to its default gateway (dashed) so quiet networks still
  read. Nodes are typed by the passive asset inventory (router / switch / AP /
  phone / IoT glyphs + vendor), carry real health states (liveness +
  host-scoped `@/health` + entity staleness — stale hosts ghost out), and show
  live ↓rx/↑tx NIC rates. Presentation is organized as **lenses**
  (Traffic / Security / L2 / Health), with subnet/role/device-group
  **collapse into meta-nodes**, an **Internet** aggregate for off-LAN traffic,
  focus mode (1–3-hop neighborhood), find:/hide: search predicates, and
  visibility filters with an honest "showing top N of M flows" label.
  Selecting a node or edge opens a **details-on-demand side panel**:
  correlator identity evidence (member claims with rule + confidence,
  passive-DNS names), a 1 h CPU sparkline, top talkers, listen sockets,
  per-direction edge rates, backing flows with per-flow **process
  attribution** ("nginx on web1 → postgres on db1"), and community-ID copy.
  Polish: hover dims everything outside the hovered neighborhood, active
  flows animate a marching dash (gated so idle networks burn no frames), a
  per-lens legend, force / ranked-grid / circular layouts, `f` zoom-to-fit,
  and pinned node positions that survive restarts.

- **Sensors split from the GUI + all-in-one sensors container image (#390).**
  `just sensors [connect=…]` and `just gui [listen=…]` replace the monolithic
  `just run` for multi-machine setups; `just image` builds a single
  `zensight-sensors` container (every sensor, correlator excluded — it stays
  the single writer of `_meta/entity/**`) whose only required knob is
  `ZENSIGHT_ZENOH_CONNECT`. Ships `scripts/gen-configs.sh` /
  `scripts/run-sensors.sh`, `docker/Dockerfile.sensors{,-runtime}`, a
  `build-docker-sensors-bundle` release job, and `docs/DEPLOYMENT.md`
  (rootful podman, host namespaces, identity mounts, quadlet units).

- **Media plane enabler (#359)**: an opaque `@media` plane for live video /
  imagery — `zensight/<proto>/<source>/@media/<stream>/…` carrying raw encoded
  bytes (Zenoh `Encoding` + frame-metadata attachment), a **plain** (non-cached)
  `Publisher::raw_media_publisher()` with a `matching_listener()` keyframe-on-
  subscribe hook, `QosClass::LiveVideo` (best-effort · drop · interactive-high),
  and stream control (`StreamControl`/`StreamDescriptor`/`StreamStatus` over
  `@/commands/stream`, `@/query/streams`, `@/status/streams`). Adds
  `Protocol::Parallax` and a frontend JPEG-preview stub. The `@media` chunk is
  invisible to both `zensight/**` and `zensight/*/@/**`, and the exporters'
  `is_telemetry_key` now rejects any `@`-prefixed chunk so media bytes never
  reach the telemetry decoders. The H.264/parallax encoder daemon is out of
  scope — this is the zenoh-side enabler.

- **Container & cloud identity evidence (#311)**: `HostEvidence` gains
  `container_id` (parsed from cgroup-v2 docker/containerd/`*.scope` paths) and
  `cloud` (`CloudFacts`: provider / instance-id / region / account, from an
  opt-in timeout-bounded IMDS probe for AWS/GCP/Azure, off by default).
  `HostEntity` gains a `container_ids` union. The correlator adds a `cloud_instance`
  merge rule (authoritative per provider, just below `host_id`) so cloned
  machine-ids still fuse when the cloud instance-id matches; `container_id` is a
  host-scoped qualifier, never a cross-host merge key. Both wire fields are
  `#[serde(default)]` for back-compat.

- **Prometheus remote-write + OTLP traces (#167)**: the Prometheus exporter gains
  a remote-write push path (protobuf + snappy POST; `remote_write: {url,
  interval, headers}`) alongside the pull endpoint. The OTel exporter gains an
  OTLP traces signal — synthesized `alert:<rule>` spans from the firing→resolved
  lifecycle with deterministic ids. Exemplars are deferred to a successor issue
  (blocked on a histogram value type). One new dep (`snap`).

- **Wire-level bandwidth-by-process tier for netring (#318, opt-in)**: joins
  netring's live flow bandwidth against the kernel socket table in-process
  (`with_flow_attribution` hook + sock_diag/`/proc` owner map refreshed off the
  hot path) and serves `BandwidthRecord{source:Netring, semantics:WireL2}` on
  `zensight/netring/@/query/bandwidth`, with an explicit `pid=-1` unattributed
  bucket. Off by default (`bandwidth_attribution`, it does `/proc` scans); the
  GUI bandwidth monitor merges it with netlink's socket-level tier. `nlink`
  unified to 0.24 across the workspace.

- **Durable storage tier + historical passive-DNS (#310)**: zenohd
  storage-manager configs persist `_meta/evidence/**` and `_meta/entity/**` to a
  `zenoh-backend-fs` volume (timestamped for mutable-key last-writer-wins), and a
  new `@pdns` plane (`zensight/@pdns/<ip>`, `PdnsRecord`) published by the
  correlator on each name-store update gives a historical IP↔name tier with a
  documented `zenoh-backend-influxdb` storage example. See `zensight-correlator/docs/storage.md`.

- **Contextual capture & bandwidth actions in the device view (#351)**: the
  netring drill-down's Capture tab now hosts the real on-demand pcap capture
  form (same shared state as the Sensors-page card — mirror, not move), gated
  on the sensor advertising the Capture kind and carrying the in-flight
  pause/resume/cancel controls; without the advert it stays health-only with
  an honest caption. The tab is visible when capture telemetry OR the advert
  is present, and selecting a netring device lazily discovers artifact kinds
  so the form works without visiting the Sensors page first. The Bandwidth
  tab gains an "Open in Bandwidth monitor" pivot that opens the global
  process/service monitor pre-scoped to the host (scope chip + clear; rows
  without a host stamp are kept visible, other hosts filtered at fold time).

- **Drill-down vertical-space redesign (#350)**: drilling into a machine now
  leads with content instead of stacked always-expanded panels.
  - The host view's two header layers (identity panel + device nav header) are
    merged into **one nav bar**: Back / prev / next / protocol icon / entity
    name / compact identity summary (`entity-id chip · live/stale · N sources ·
    M IPs`) / metric count / exports. The identity facts + resolution-group
    drill-down collapse behind a ▾/▸ "identity" toggle (persisted,
    collapsed by default); expanding still shows every fact and member claim.
  - The syslog drill-down no longer renders its own second Back button /
    duplicate header — the facet body is a slim toolbar (message count +
    filter toggle) under the shared bar.
  - The logs facet's statistics (severity summary + rollups) sit behind one
    collapsible **"Log statistics"** card (default closed); the rollup is a
    compact KPI tile row (errors / warnings / units-in-failure / journald
    throughput, via the shared `kit::metric_tile`) and the by-unit list shows
    top-3 with a "Show all N" affordance instead of always 10.

- **Frontend `link_profile` + subscription scope (#364)**: the GUI Settings →
  Zenoh section gains a *Link profile* picker (`standard` | `constrained`) and a
  *Subscription scope* field (comma-separated key expressions replacing the
  `zensight/**` firehose; empty = everything). `constrained` declares **plain**
  telemetry subscribers — no AdvancedSubscriber history burst or recovery traffic
  on a lossy/slow link — and back-fills the Logs view from the local redb store
  on connect instead. Scope/profile changes hot-restart the Zenoh session like
  connection edits do. Control-plane subscriptions (health, alerts, entities)
  are unaffected by scoping. Completes the R6 half deferred from #357.

- **netring runtime threat-intel hot-reload (#328).** A new
  `@/commands/threat_intel` channel (status on `@/status/threat_intel`) swaps the
  live IOC set (`set_ioc` / `reload_ioc_files` / `clear_ioc`) and YARA rules
  (`set_yara`, behind the new `--features yara` flag) into the running monitor via
  its `ReloadHandle` — no capture restart. A bad YARA source is rejected with a
  compile error in the status reply while the previous rules keep scanning. The
  GUI Security view gains a *Threat Intel* panel (paste indicators / rules, reload
  configured files, armed/loaded readout). New `threat.reload` config arms the
  matchers even on an empty start so runtime reload works; `threat.yara.file`
  compiles startup rules. Off by default (matchers armed only when config already
  provides indicators).

- **Zenoh-efficiency core for low-bandwidth / unreliable links (epic #352,
  `docs/design/zenoh-efficiency.md`)**: a coherent "resilient links" pass across the bus.
  - **Per-traffic-class QoS** (`zensight_common::QosClass`, #353): telemetry and
    health are best-effort + drop + low priority (a lost sample is superseded);
    alerts, commands, evidence and entities are **reliable + block** at higher
    priority. `express` is off everywhere (batching beats latency on a constrained
    link). **This fixes a correctness bug**: alerts previously published via a
    plain drop `put`, so a firing/resolved event or its delete tombstone could be
    silently dropped on a lossy link, stranding a live GUI in a stale state.
  - **Declare every publisher; ban raw `session.put`** (#356): new
    `zensight_common::PublisherRegistry` (declare-on-first-use + per-key cache +
    QoS); all sensors, the control plane, and the frontend command/artifact path
    publish through a declared publisher (interned key + primed routing). A CI
    guard fails the build on any raw `session.put`/`session.delete` in-scope.
  - **Right-sized AdvancedPublisher** (#354): `cache_only` registries no longer
    attach sample-miss-detection or a 500 ms/key heartbeat (the builder now honors
    its config bools; telemetry heartbeat default relaxed 500 ms → 5 s); the
    correlator entity publisher downgraded to a plain declared publisher.
  - **Configurable exporter subscription scope** (#357): `filters.key_expr` on
    both exporters narrows the telemetry subscription (default `zensight/**`) so
    unwanted protocols and the `_meta/**` control plane never reach the exporter
    over the wire. (Frontend `link_profile` half split to #364.)

- **netring passive-inventory enrichment from flowscope 0.22 (#329)**: the netring
  asset inventory (`@/query/assets`) is widened with a classified device role
  (router / switch / access-point / phone / iot / host), first-seen timestamp,
  source-count confidence, the full hostname set, per-parser fingerprints (JA3 /
  JA4 / HASSH / p0f), and — on `ja4plus` builds — x509 subject/SANs; the seen-via
  decode gains the 0.22 TLS/SSH/p0f handshake sources. The GUI Inventory view adds
  a role filter chip row, a first-seen sort, source-count + fingerprint-pivot
  columns, and a `--demo` mock fleet so the enriched inventory is developable
  without live capture. All wire additions are `#[serde(default)]`.

- **netring encrypted-traffic frontier from netring 0.29 (#326)**: the netring
  sensor adopts netring 0.29's typed encrypted-traffic handlers. QUIC and SSH swap
  to `on_quic_fingerprint` / `on_ssh_fingerprint` (deleting the hand-rolled
  banner+KEXINIT correlation), surfacing QUIC JA4 (royalty-free `q`-prefixed) + PQ
  key-share + app-protocol and both client/server HASSH + KEXINIT algorithms. TLS
  fingerprints gain a post-quantum key-share flag, aggregated into a streamed
  `tls/pq_ratio` PQ-readiness gauge with a GUI badge/stat. New `collect.encrypted_dns`
  classifies DoT/DoQ/DoH from the handshake into streamed `dns/encrypted/*` counts +
  an `@/query/encrypted_dns` inventory (GUI "Encrypted DNS" panel), and
  `anomalies.encrypted_dns_bypass` (+ optional `dns_resolver_allowlist`) fires an
  `encrypted_dns_bypass` anomaly (ATT&CK T1572) for sessions to un-sanctioned
  resolvers. New `collect.ip_reassembly` reassembles IP fragments before L7 parsing.

- **netlink sockdiag depth from nlink 0.24 (#322)**: the netlink sensor adopts
  three nlink 0.24 sockdiag features. Per-rule nftables counters now decode via
  nlink's native `RuleInfo::counter()` — the hand-rolled `NFTA_RULE_EXPRESSIONS`
  TLV parser (#115) is deleted. `@/query/sockets` `SocketRecord`s gain structured
  congestion-control fields (`bbr_bw_bps`, `cc_min_rtt_us`) via the `with_cc_info()`
  extension, so BBR bottleneck bandwidth + min-RTT surface per socket (with a GUI
  column). A port-filtered sockets query compiles the selector to kernel-side
  INET_DIAG bytecode (`FilterExpr`, local-OR-remote port matching), cutting dump
  volume on busy hosts while keeping the client-side match as a backstop.

- **Bandwidth live monitor (#319, epic #320)**: a new bmon/nethogs-style
  **Bandwidth** view (nav rail) with two modes — **Processes** (per-process rows
  fetched from the netlink `@/query/bandwidth` channel) and **Services**
  (per-service rows derived from streamed systemd `unit/<name>/ip_*_bps`, with a
  live sparkline). Every row carries a **source/semantics badge** (e.g.
  `sock_diag · goodput`, `systemd · wire-L3`) and a legend so app-goodput and
  wire-L3 rates are never silently compared; the explicit `unattributed` bucket is
  shown, not dropped. Sortable/filterable table; `--demo` populates both modes
  (Services from the demo stream, Processes from a mock since demo serves no
  queryables).
- **Per-process TCP bandwidth from sock_diag (#317, epic #320)**: the netlink
  sensor derives per-process network rate from `tcp_info` goodput byte counters
  (`bytes_acked`/`bytes_received`), sampled per socket **cookie** and served
  query-only on `@/query/bandwidth?top=N` as ranked `BandwidthRecord`s — never as
  high-cardinality streamed per-pid keys. Unprivileged and **TCP-only**
  (`udp_diag` has no per-socket byte counters); records are tagged
  `bw.source=sock_diag`/`bw.semantics=app-goodput`/`bw.proto=tcp` so the honest
  limits (below-wire goodput, short-flow misses, TCP-only) travel with the data.
  Unattributed sockets fold into one explicit bucket rather than being dropped.
  `SocketRecord` gains `bytes_acked`/`bytes_received`/`bytes_sent`.
- **Bandwidth-by-service from systemd IPAccounting (#315, epic #320)**: the systemd
  sensor derives per-unit network rate `unit/<name>/{ip_ingress_bps,ip_egress_bps}`
  from successive `IPIngressBytes`/`IPEgressBytes` deltas (the cheapest bandwidth-by-*
  tier). Metrics are labelled `bw.source=systemd`/`bw.semantics=wire-l3` (cgroup_skb:
  L3+ bytes, no L2) so they're never blended with app-goodput or wire-L2 sources; a
  unit restart re-baselines the counter; an active unit with IPAccounting off emits an
  explicit `ip_accounting=false` state rather than a silent zero. New shared vocabulary
  in `zensight-common::bandwidth` (`BandwidthSource`/`ByteSemantics`/`ProtoScope`,
  `BandwidthRecord`, and the `bw.*` label keys) underpins all bandwidth tiers.

- **Host-identity envelope (#301)**: every sensor now publishes a registration
  record on `zensight/_meta/sensors/<name>/<source>` and a self-report
  `HostEvidence` claim on `zensight/_meta/evidence/host/<sensor>/<source>`
  (re-emitted every 60 s via cached publishers). The identity carries a
  **hashed** machine-id (`host_id` = sha256(machine-id + app salt); the raw id
  never leaves the host), boot id, hostname/fqdn, and non-loopback IPs/MACs.
  Health snapshots gain `host_id`; alerts gain a `host.id` annotation label.
- **sysinfo process enrichment + argv scrubber (#302)**: the on-demand
  `@/query/processes` `ProcessRecord` gains `cmdline`, `exe`, `ppid`, `cgroup`
  (v2 path — joins a process to its systemd unit), `start_time` (the
  `(pid, start_time)` identity pair), and `user`. Command lines are **scrubbed of
  secret-looking argv values** (Datadog-style key list; both `key=value` and
  `--key value` shapes) and byte-capped before publish — controlled by
  `processes.scrub_args` (default `true`), `custom_sensitive_words`, and
  `strip_proc_arguments`.
- **systemd/logs unit↔process↔log identity (#303)**: `UnitDetail` (`@/query/unit`)
  gains `main_pid` + `main_pid_start_time`, `invocation_id`, and `control_group`;
  the logs sensor captures `_SYSTEMD_INVOCATION_ID` as `sd.journald.invocation_id`.
  Together these join a systemd unit to its main process, its cgroup, and its exact
  log lines.
- **netlink socket→process attribution (#304)**: `@/query/sockets` `SocketRecord`
  gains `cookie`, `cgroup_id`/`cgroup`, and the owning `pid`/`process`/
  `proc_start_time`, resolved **unprivileged** via a per-request `/proc` fd-scan
  (`collect.socket_processes`, default on; ceiling `socket_process_max_procs`,
  default 4096). An optional eBPF tier attributes recently-closed / live-established
  sockets the fd-scan can't reach.
- **Passive DNS name resolution (#308)**: the netring sensor parses DNS answers
  (flowscope `NameMap` — CNAME-chain-following, glue-poisoning-safe, PTR-aware)
  into a client-scoped IP↔name cache. Flow and talker records gain
  provenance-ranked `dst_names`/`names`, and an FQDN-pivoted RITA beacon detector
  flags periodic beaconing keyed by destination name (ATT&CK T1071).
- **Identity evidence feeds (#307)**: netring publishes observed-asset evidence
  (ARP/LLDP/DHCP inventory → `HostEvidence` with `observer=netring`) and
  passive-DNS `NameObservation`s; netlink publishes observed-neighbor evidence
  (ARP/ND table → `HostEvidence`). All third-party claims are rate-limited
  (per-source min-interval + per-tick cap) and age out by TTL.
- **`zensight-correlator` — identity correlation service (#305)**: a new
  single-writer daemon that subscribes to the evidence keyspace and merges
  claims into `HostEntity` docs on `zensight/_meta/entity/host/<id>` via a
  deterministic union-find over ranked identity rules (host_id > MAC+IP > FQDN >
  hostname; IP/MAC-alone never join; a host_id-conflict guard blocks weak
  false-merges). Entities carry membership provenance (which rule + confidence
  bound each source), are re-emitted for liveness, tombstoned on retire, and
  seeded to late joiners via the `_meta/query/entities` queryable; arbitrary-IP
  names resolve on demand via `_meta/query/names?ip=`. A `--demo` mode feeds
  synthetic evidence through the real pipeline. A Zenoh liveliness token
  guarantees a single writer.
- **Host-entity frontend (#306)**: the GUI consumes the correlator's `HostEntity`
  docs (subscribe `_meta/entity/**` + connect-time seed) into an `EntityStore`.
  The dashboard groups a host's per-protocol devices under **one host card**
  (worst-of-members status, facet chips, alert rollup, a persisted "group by host"
  toggle); the host detail page gains an identity header and a "merged from N
  sources" resolution drill-down (each member's binding rule + confidence); the
  topology keys nodes by entity, bridges wire flows via identifying IPs, and shows
  wire-only hosts as passive nodes. With no correlator the store is empty and every
  view falls back to the per-source rendering (degraded path, pinned by test).
- **On-demand pcap capture (#333)**: the netring sensor serves a `Capture` artifact
  over the `@/artifact` channel — a dedicated build-time packet tap (idle cost: one
  `ArcSwap` load per matching frame) is narrowed per request via the monitor's
  reload handle, streamed through a bounded drop-on-overflow channel into a pcap
  writer, and zstd-compressed to a `capture-<source>-<ts>.pcap.zst` blob. Requests
  are clamped to configured `capture.on_demand` limits (duration, bytes, snaplen,
  cooldown, optional filter allowlist); off by default. The GUI's artifact card
  gains a capture request form (duration/filter/size/compress) with progress and
  download; the netring device screen's tab is renamed **Capture health** and points
  to the Sensors page for launching captures.

### Changed

- **BREAKING (efficiency, #355): default serialization is now CBOR.**
  `Format::default()` flips JSON → CBOR, so every sensor/exporter/config that
  didn't pin a format now encodes CBOR on the wire (smaller envelopes on a
  constrained link). All on-bus consumers decode format-agnostically
  (`decode_auto` sniffs JSON vs CBOR by first byte), so mixed-format fleets during
  a rolling upgrade keep working; set `serialization: "json"` to opt back in.
- **BREAKING (efficiency, #353): alert/command traffic is now reliable+block.**
  Alerts, commands, evidence and entities publish with `QosClass` (reliable +
  block); telemetry/health are best-effort + drop. See the Added section.
- **BREAKING (identity, #301): host-id config unified to `source`.** The
  netlink `netlink.hostname`, netring `netring.sensor_id`, and sysinfo
  `sysinfo.hostname` config fields are renamed to `source` (same `"auto"` →
  local-hostname default). The remote-device sensors (snmp, gnmi, modbus,
  netflow, logs) gain an optional `source` override for the *agent host* id
  used in debug bundles and artifact routing. Update your JSON5 configs.
- **BREAKING (identity, #301): `SensorInfo` redesigned and re-keyed.** The
  (previously never-published) `zensight/_meta/sensors/<name>` record moves to
  `zensight/_meta/sensors/<name>/<source>` — the per-name key collides across
  hosts — and now carries identity fields instead of duplicated health data.
  The dead `zensight/_meta/correlation/<ip>` keyspace and `CorrelationEntry`
  wire type are deleted (replaced by `_meta/evidence/**` + the upcoming
  entity keyspace).
- **Alert keys ignore `host.`-prefixed labels** (the annotation namespace):
  identity metadata stamped onto alerts never changes alert identity, so
  firing/resolve pairs stay matched across identity refreshes. Alert keys for
  alerts without such labels are unchanged.

- **BREAKING (large-data transfer, #332): unified the `@/report` and `@/snapshot`
  control-plane channels into one `@/artifact` channel.** Operator-facing
  migration note — anything that PUT report/snapshot requests or GET the bytes
  must move to the new keyspace and wire types:
  - **Keyspace**: `@/report/{request,status,blob/<id>/**,cancel}` and
    `@/snapshot/{request,status,cancel}` collapse to
    `@/artifact/{request,status,cancel}` + `@/artifact/blob/<id>/**`. The Tier-2
    `@/store/<algo>/<hash>` and `@/tree/<id>` queryables are unchanged but are now
    kind-agnostic delivery infra shared by any artifact whose producer emits a
    `Tree` delivery.
  - **Wire types** (`zensight-common`): `Report*`/`Snapshot*` → `Artifact*` —
    `ReportRequest`/`SnapshotRequest` → `ArtifactRequest` (+ tagged `ArtifactKind`:
    `Report`, `Snapshot { dir }`, `Capture` — the last shipped by netring in
    #333); `ReportState` → tagged
    `ArtifactState`; the new tagged `Delivery` (`Blob` | `Tree`) tells the client
    which tier to pull; `ReportStatus`/`SnapshotStatus` → `ArtifactStatus { kinds:
    Vec<KindStatus> }` (one entry per kind). The old `report_*`/`snapshot_*` key
    builders are replaced by `artifact_{request,status,cancel}_key` +
    `artifact_{blob,store,tree}_prefix`.
  - **Config**: the top-level `report:` / `snapshot:` sections (`ReportLimits` /
    `SnapshotLimits`) move under a single `artifacts: { report, snapshot }` section
    (`ArtifactLimits`); every kind stays **disabled by default**. `SnapshotDir {
    name, path }` allowlist entries now live under `artifacts.snapshot.dirs`.
  - **Sensor-core API**: `SensorRunner::with_report`/`with_snapshot` →
    `with_artifacts(source_id, vec![ReportProducer, SnapshotProducer, …])`;
    `SensorConfig::report_limits`/`snapshot_limits` → `artifact_limits()`. One
    `ArtifactChannel` owns request/status/cancel + reaper (per-kind busy +
    cooldown, lazy `BlobServer`/`TreeServer`); producers implement the
    `ArtifactProducer` trait. See `docs/KEYSPACE.md` §3.1a and
    `docs/design/large-data-transfer.md`.
  - **Frontend**: the `blob_fetch.rs` + `dir_fetch.rs` views merge into one
    `zensight/src/view/artifact_fetch.rs` whose `download_stream` matches on
    `Delivery`.
- **Dependencies**: bumped `nlink` 0.23 → 0.24, `netring` 0.28 → 0.29, and
  `flowscope` 0.20 → 0.22 (netlink and netring sensors). Migrated the breaking
  surface: `MonitorBuilder::flow_risk()` → `flow_analysis()`, and our local
  `DetectorScore` impls (`RitaBeaconHit`, `FloodScore`) to the typed
  `DetectorKind` (`DetectorKind::Other(...)`). Published anomaly kind slugs were
  byte-identical at this step (later renamed by #369, above).
- **BREAKING (metric value corrections, #321)**: the dependency adoption above
  fixed several netlink/netring metrics that were emitting wrong values — zeroed
  `tcp_info` fields, an interface-mask off-by-one, and mis-parsed ICMPv6 counters.
  The keys are unchanged but the values change; dashboards/alerts calibrated
  against the old (incorrect) numbers should be re-checked.

### Fixed

- **netring RED latency/duration percentiles used unbounded per-window sample
  buffers (#325).** The DNS query-RTT, HTTP request→response latency and
  flow-duration percentiles each accumulated every sample of the window into a
  `Mutex<Vec<u64>>` (soft-capped at 100k–1M entries) and sorted it from scratch
  each aggregate tick. They now feed a bounded DDSketch (`RedSketch`: ~512
  log-spaced bins, 1% relative error, O(1) insert) that is read + reset each
  tick — constant memory regardless of DNS/HTTP/flow rate, no per-tick sort. The
  published `dns/query_rtt_p{50,95,99}_ms`, `http/latency_p{50,95}_ms` and
  `flow/duration_p{50,95}_ms` keys are unchanged (values now approximate within
  the sketch's 1% relative error). Adopting netring's rolling `red()` flow-RED
  signal and the talkers/matrix `aggregate()` swap are deferred follow-ups on
  #325 (additive / response-shape changes, not the memory bug).

- **netring beacon / port-scan detectors were systematically under-detecting
  source-port-rotating activity (#324).** The RITA/CV beacon detectors keyed
  their state on the full 5-tuple, so a beacon that opens a fresh connection
  (new ephemeral source port) for each ping fragmented into N one-flow series
  that never accumulated enough samples to score; the port scanner keyed the
  same way. They now key detector *state* on `HostPair` (src, dst, dst-port) and
  `SrcHost` (scanner IP) respectively, collapsing rotating-port activity into one
  series — the real curl-in-a-loop / C2 shape is now caught. Beacons also now
  observe once per connection (`FlowEnded`) instead of per packet (the correct
  ping granularity). The emitted alert still carries the triggering flow's full
  5-tuple + Community ID, so the alert schema is unchanged. Pinned by a
  regression test that shows the old 5-tuple keying misses the same series. The
  hand-rolled Community-ID v1 hash was replaced by flowscope's (byte-identical).

These are upstream bug fixes inherited with the bump; they change values on
metrics ZenSight already publishes, so dashboards and alerts on these series
will see a step:

- **netlink `sockets/tcp/bytes_retrans_total` and `.../reordered_total` were
  always zero.** `nlink` < 0.24 stopped parsing `TcpInfo` at byte 168, so
  `bytes_retrans` and `reord_seen` silently read 0 on every kernel. They now
  carry real values.
- **netlink socket memory metrics never appeared.** An off-by-one in
  `nlink`'s `InetExtension::mask()` meant `with_mem_info()` actually requested a
  different extension, so `InetSocket.mem_info` was always `None` and the skmem
  branch was dead. Socket-memory metrics now flow for the first time.
- **netring IPv6 ICMP error counters were wrong.** `flowscope` misdetected
  ICMPv6 error types (Destination Unreachable, Time Exceeded) as ICMPv4, so the
  `on_icmp_error` counters (unreachable / time-exceeded / MTU) undercounted or
  mislabeled IPv6 errors. Counts are now correct.
- **netring AF_XDP could hang under sparse traffic.** `flowscope`'s async
  AF_XDP poller could miss a wakeup when packets arrived during an idle gap;
  fixed upstream.
- **netlink XFRM/IPsec polling no longer spams the kernel log.** `nlink`'s XFRM
  dumps appended a stray struct that made the kernel log a ratelimited
  `netlink: … bytes leftover` warning on every poll. Results were always
  correct; the log noise is gone.

## [0.6.2] - 2026-06-27

### Fixed

- **Packaging**: build the legacy sensor Docker images (syslog/sysinfo/snmp).
  `Dockerfile.sensor` gained `libsystemd` (build + runtime) for the logs sensor's
  journald support and `libssl3` at runtime for snmp; the Docker matrices no
  longer `fail-fast`. Completes the container-image set (deb/rpm/flatpak and the
  exporter images were already published for 0.6.1). No shipped binary changed.

## [0.6.1] - 2026-06-27

### Fixed

- **Packaging**: restore the RPM and Docker artifacts in the release workflow.
  The Fedora RPM build now installs `protobuf-devel` (the well-known-type
  includes gNMI's build needs), and both Docker build contexts
  (`Dockerfile.sensor` / `Dockerfile.exporter`) now copy the
  `zensight-sensor-netlink` / `zensight-sensor-netring` crates needed for
  workspace resolution. No shipped binary changed from 0.6.0; this only completes
  the artifact set (deb + flatpak were already published for 0.6.0).

## [0.6.0] - 2026-06-27

A large release: two new kernel/wire-level sensors, a unified logs sensor with
journald, a full host/incident-centric frontend redesign with NDR, alert export
to Prometheus/OTel, and OS packaging. See `docs/README.md`, `docs/KEYSPACE.md`,
and `docs/ARCHITECTURE.md` for the authoritative references.

### Added

#### New sensors

- **`zensight-sensor-netlink`** — Linux kernel networking telemetry over
  RTNETLINK + `sock_diag`, read **unprivileged**: interface/address/route/
  neighbor state, enriched `tcp_info` (delivery/pacing/retrans/reordering),
  qdisc/bufferbloat health score with AQM classification, conntrack and
  WireGuard (root-gated), nftables per-rule hit-rate, a default-route flap
  history, and a control-plane change timeline. Embeds a **sentinel** that
  asserts declared expectations (sockets/links/routes, rate-of-change, delivery
  floors) and raises alerts on deviation, hot-swappable at runtime.
- **`zensight-sensor-netring`** — wire-level flow / L7 / NDR telemetry via
  AF_PACKET/AF_XDP (needs `CAP_NET_RAW`) or offline pcap replay: flow RED,
  bandwidth, TCP resets, DNS/HTTP RED, TLS fingerprints, ICMP errors, a
  `(src,dst)` traffic matrix, and capture self-health with honest drop
  accounting + overload detection. Detectors: TRW port-scan, RITA beaconing,
  DNS-tunnel / Newly-Observed-Domain, connection-flood, Community ID v1, and
  MITRE ATT&CK technique tags. Opt-in: lateral-movement (SMB/RDP/Kerberos) and
  data-exfil heuristics, threat-intel (flow-risk/IOC/Sigma), passive asset
  inventory (ARP/NDP/LLDP/CDP), QUIC/SSH inventories, and JA4H fingerprints.

#### Logs sensor (formerly `syslog`)

- **journald ingestion** via libsystemd — scope/namespace, server-side matching,
  cursor-based gap-free resume, and known-event alerts (coredump / unit-failed /
  OOM by `MESSAGE_ID`); audit/SELinux records tagged `category=security`.
- **Per-line log events** (`events/<uid>`) with the OpenTelemetry logs data
  model in labels, replacing the last-writer-wins `<facility>/<severity>` key.
- Multiline stack-trace joining, a Drain3-style streaming **template miner** with
  novelty / rate-spike detection, derived per-unit log-rate and error rollups,
  per-unit **error budgets / SLOs with burn-rate alerts**, journald backpressure
  / rate-limit / drop accounting, and RFC 6587 framing on the network path.

#### Alerting & detection

- Common **alert model** (`Alert{Kind,Severity,State}`, stable `alert_key`) and
  an `AlertReporter` (debounce, reconcile) in `zensight-sensor-core`. Alerts flow
  on `@/alerts/<key>` as a firing → resolved → tombstone lifecycle, with a
  `@/query/alerts` firing-set queryable for late-joiner recovery.

#### Frontend

- **Redesign**: persistent app shell (left nav rail + top bar), host/
  incident-centric information architecture with facet tabs, a unified
  **Incident** object (grouped alerts + timeline + evidence pivots), and a
  composite host-health / worst-first fleet overview.
- New views: **Security** (NDR anomaly + ATT&CK by-tactic lens, detection
  tuning), **Expectations** (sentinel authoring), **Sensors** (health/failure
  tracking), top-level **Logs** (structured drill-down, MESSAGE_ID catalog,
  follow/pause, boot lens), **Inventory** + unified **fingerprint explorer**, and
  specialized netlink/netring device views with on-demand detail drill-downs.
- Productivity: **command palette** (Ctrl+P), **fuzzy** global metric search,
  **keyboard-shortcuts help overlay**, saved **alert-filter presets**, alert
  severity/source filter pills, per-device **metric favorites**, "alert on this
  metric" promotion, desktop notifications for CRITICAL alerts, native save
  dialog for export, and an absolute from/to chart time-range picker.
- Topology enrichment (netlink host nodes + neighbor-adjacency edges, alert
  overlay, router classification), a universal trend layer (booleans as 0/1 step
  series, log-rate series), and a **local store** (redb hot ring + tiered
  retention/eviction, template-aware log sampling) so history survives restart.

#### Exporters

- **Export sensor alerts** to Prometheus (a `<prefix>_alert` gauge, Alertmanager-
  compatible) and OTel (OTLP log records on the `zensight.alerts` scope).
- OpenTelemetry **host-metrics semantic-convention** mapping for sysinfo via a
  shared `zensight_common::semconv` table, so exported metrics are
  dashboard-portable.

#### Packaging & operations

- **systemd units** for every sensor and exporter (hardened: `DynamicUser`,
  `ProtectSystem=strict`, minimal ambient caps) plus **deb/rpm packaging parity**
  for all sensors and exporters, installing a unit and an example config.
- **SIGTERM** is handled for graceful shutdown (publish offline status, tombstone
  firing alerts) under systemd/Docker stop, not just Ctrl-C.

#### Project

- `justfile` to build / grant caps / configure / run the GUI with local sensors,
  pinning an explicit loopback rendezvous so discovery works without multicast.
- CI **clippy (`-D warnings`) + rustfmt gate** and a design-system color guard.

### Changed

- **BREAKING**: Renamed the "bridge" crate family to "sensor". `zenoh-bridge-*`
  crates/binaries are now `zensight-sensor-*`; `zensight-bridge-framework` is now
  `zensight-sensor-core`. Framework types renamed (`BridgeRunner`→`SensorRunner`,
  `BridgeConfig`→`SensorConfig`, `BridgeArgs`→`SensorArgs`, `BridgeHealth`→
  `SensorHealth`, `BridgeError`→`SensorError`, `BridgeInfo`→`SensorInfo`,
  `BridgeStatus`→`SensorStatus`).
- **BREAKING (wire)**: Renamed the `_meta/bridges/*` discovery key to
  `_meta/sensors/*`, and the `bridge`/`bridges` JSON fields in `HealthSnapshot`,
  `SensorInfo`, and `CorrelationEntry` to `sensor`/`sensors`. All sensors and the
  frontend cut over together; the `zensight/<protocol>/<source>/<metric>`
  telemetry prefix is unchanged.
- **Keyspace v2**: a formalized control-plane under `zensight/<protocol>/@/…`
  (`health`, `errors`, `status`, `alive`, `alerts`, `commands`, `query`) that
  telemetry wildcards deliberately don't match, plus on-demand `@/query/<topic>`
  detail channels (high-cardinality data served on request, never streamed). The
  `syslog` protocol is now `logs`. Documented in `docs/KEYSPACE.md`.
- Telemetry is published with zenoh-ext **AdvancedPublisher** (per-key cache +
  late-joiner recovery), paired with the GUI's AdvancedSubscriber.
- Frontend **design system**: type/spacing tokens, a theme-aware color layer, and
  a shared component kit; all ad-hoc colors centralized (CI-guarded).

### Fixed

- **Discovery**: the GUI and sensors form a session via an explicit loopback
  rendezvous instead of relying on multicast (broke under VPN/extra interfaces).
- Harden the SNMP authPriv path so a malformed v3 config returns an error instead
  of panicking; correct gNMI path-segment handling.
- Device-liveness regression and several dead/un-wired query channels in the GUI.

## [0.5.0] - 2026-02-21

### Fixed

- **Critical**: Remove unsafe `transmute` in AdvancedPublisher registry, replaced with safe `Arc` cloning
- **Critical**: Fix TOCTOU race condition in publisher cache with atomic check-and-insert
- **Critical**: Add missing `Sysinfo` protocol variant to `parse_key_expr()`
- **Data Integrity**: Fix `i64` to `f64` precision loss in `TelemetryValue::From<i64>` conversion
- **Data Integrity**: Tag `TelemetryValue` enum with `#[serde(tag)]` for unambiguous serialization
- **Exporters**: Fix silent metric rendering failures in Prometheus collector
- **Exporters**: Fix silent export failures in OTEL exporter
- **Exporters**: Fix gauge key collision with sorted attributes in OTEL exporter
- **Bridges**: Fix gNMI nanosecond timestamp conversion overflow
- **Bridges**: Fix Modbus address overflow with checked arithmetic
- **Bridges**: Fix incomplete regex escaping in syslog `glob_to_regex()`

### Changed

- `parse_key_expr()` now returns `Result` with descriptive errors instead of `Option`
- `KeyExprBuilder::build()` validates inputs (no empty strings, no invalid chars)
- Replace string-typed status fields with `HealthStatus` enum in bridge health
- `errors_last_hour` now uses a rolling window instead of monotonic counter
- Handle lock poisoning gracefully in `CorrelationRegistry`
- Improved error categorization for Zenoh errors (`BridgeError` variants)
- gNMI reconnection uses exponential backoff (5s to 5min) instead of fixed 5s
- Reduced NetFlow mutex contention by narrowing lock scope
- Dashboard uses cached filtered results for better performance
- Metric history uses `VecDeque` instead of `Vec` for efficient bounded storage
- Reduced string allocations in subscription key expression parsing

### Added

- **Toast Notifications**: Non-intrusive notification system for user feedback
- **Loading Indicator**: Visual feedback during Zenoh connection establishment
- **Stale Metric Indicators**: Visual cue for metrics that haven't updated recently
- **Decode Failure Metrics**: Both exporters now track deserialization error counts
- **OTEL Staleness Cleanup**: Automatic expiry of stale gauge entries in OTEL exporter
- **OTEL Instrument Caching**: Cache `Meter` and `Logger` instances to avoid recreation

## [0.4.0] - 2025-12-29

### Added

- **Device Metrics Table**: Replace metrics list with Iced 0.14 table widget for better data presentation
- **Page Transition Infrastructure**: Add animated page transitions between views
- **Dashboard Table View Toggle**: Switch between card and table views on dashboard
- **Syslog Table Widget**: Replace log stream with Iced 0.14 table widget
- **Responsive Grid Layout**: Dashboard device cards now use responsive grid
- **Double-Click Support**: Navigate to device details with double-click on cards
- **Animated Status Indicators**: Status dots use iced_anim for smooth animations

## [0.3.0] - 2025-12-29

### Added

- **Prometheus Exporter** (`zensight-exporter-prometheus`): Export ZenSight telemetry to Prometheus
  - HTTP `/metrics` endpoint for Prometheus scraping
  - Automatic metric type conversion (Counter, Gauge, Text to Prometheus types)
  - Metric name sanitization for Prometheus compatibility
  - Staleness-based expiry to prevent unbounded memory growth
  - Configurable filtering by protocol, source, and metric patterns

- **OpenTelemetry Exporter** (`zensight-exporter-otel`): Export ZenSight telemetry via OTLP
  - Support for both gRPC and HTTP OTLP protocols
  - Exports metrics and logs signals
  - Syslog messages converted to OTEL logs with severity mapping
  - Resource attributes for service identification

- **CI/CD**: Added deb, rpm, and Docker builds for exporters

### Changed

- Unified workspace versioning for all crates

## [0.2.0] - 2025-12-28

### Added

- **Network Topology View**: Interactive force-directed graph visualization
  - Canvas-based rendering with zoom and pan
  - Node search and click-to-select
  - Edge thickness based on bandwidth
  - Info panel with device details

- **UI Animations**: Smooth transitions using iced_anim
  - Animated buttons with hover effects
  - Animated SVG icons

- **Syslog Filtering**: Advanced message filtering capabilities
  - Static filters (severity, facility, patterns) in config
  - Dynamic runtime filters via Zenoh commands
  - Frontend filter panel

- **Advanced Zenoh Features**
  - Liveliness tokens for bridge/device presence detection
  - AdvancedPublisher/Subscriber from zenoh-ext

- **Cross-Bridge Infrastructure**
  - Bridge health monitoring (`BridgeHealth`)
  - Device liveness tracking (`DeviceLiveness`, `DeviceStatus`)
  - Unified error reporting (`ErrorReport`, `ErrorType`)
  - Cross-bridge correlation registry

- **Enhanced Sysinfo Bridge**
  - CPU breakdown (user/system/iowait/steal/nice/idle/irq/softirq)
  - Disk I/O stats (read/write bytes, IOPS)
  - Temperature sensors (Linux hwmon)
  - TCP connection state counts

- **Demo Mode Enhancements**
  - Realistic telemetry simulation
  - Health and liveness simulation
  - Periodic anomaly injection

- **Persistence**: Save/restore alert rules, theme, and current view

- **Chart Improvements**
  - Multi-metric comparison mode
  - Threshold/baseline lines
  - Larger time windows (6h, 24h, 7d)
  - Zoom with keyboard and Ctrl+scroll
  - Pan controls for time navigation

- **Alerts**: Test Rule button for previewing matches

- **UI Polish**
  - Tooltips for truncated values
  - Alert count badge on dashboard
  - Light/dark theme toggle
  - Keyboard shortcuts (Ctrl+F search, Esc back/close)
  - Search debouncing (300ms)

### Fixed

- Node click detection in topology view
- Theme-aware colors (replaced hardcoded values)
- Layout convergence stability
- Clippy warnings across workspace

## [0.1.0] - 2025-12-15

### Added

- **Core Platform**
  - `zensight`: Iced 0.14 desktop frontend
  - `zensight-common`: Shared telemetry model and Zenoh helpers
  - `zensight-sensor-core`: Common bridge infrastructure

- **Protocol Bridges**
  - `zensight-sensor-snmp`: SNMP v1/v2c/v3 with full USM support, MIB loading
  - `zensight-sensor-syslog`: RFC 3164/5424, UDP/TCP/Unix socket
  - `zensight-sensor-netflow`: NetFlow v5/v7/v9 and IPFIX
  - `zensight-sensor-modbus`: Modbus TCP/RTU
  - `zensight-sensor-sysinfo`: System metrics (CPU, memory, disk, network)
  - `zensight-sensor-gnmi`: gNMI streaming telemetry with TLS

- **Frontend Features**
  - Dashboard with device overview
  - Device detail view with metrics
  - Time-series charts
  - Alerts and notifications
  - Settings page
  - Data export (CSV/JSON)
  - SVG icons

- **Testing**
  - Simulator-based UI tests
  - Mock telemetry generators
