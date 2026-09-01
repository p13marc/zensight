# Changelog

All notable changes to ZenSight will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **The systemd sentinel joins `@desired`** (#849, the first of three). Its
  expectation set can now be authored fleet-wide on
  `v1/@desired/state/<host>/systemd/expectations` — LWW, storage-backed,
  reconciled on connect and reconnect — with
  `state/systemd/applied/expectations` saying which of `file | desired | rpc`
  is actually in force. The seam is hostspec's (#816), unchanged.

  The blocker was RFC 08 §7's schema gate: a state-class payload needs a real
  schemars-generated schema, and a sensor-crate type can never provide one
  (`zensight-common` cannot depend on a sensor, so `describe` could only carry
  a summary stub — #815's gate refused exactly that). So the expectation
  vocabulary moved to `zensight-common::systemd`, as hostspec's did. Checking
  logic stayed in the sensor; these are data.

  **No breaking registry change was needed**, contrary to the issue's plan. It
  anticipated an `ExpectationsConfig` name collision between the systemd and
  netlink sentinels, requiring a retire-and-sibling or a coordinated
  force-relock. There is no collision: netlink's `expectations/set` takes
  `ExpectationCommand` and logs' `rules/set` takes `LogRulesConfig`, so
  `ExpectationsConfig` is systemd's alone. The belief came from a *description
  string* in the schema table, not from any binding. Both registry additions
  relocked as purely additive.

### Fixed

- **`@rpc/systemd/expectations/set` accepts the shape it advertises** (#849).
  The registry has declared this request as `ExpectationsConfig` — the plain
  expectation set — since 1.0, which is what hostspec's equivalent accepts and
  what `@desired` carries. The sensor only ever accepted the tagged
  `{"type": "set_expectations", …}` envelope the GUI happens to send, so a
  fleet tool that built its body from `describe` was refused by the very sensor
  that had told it what to send. Both shapes are now accepted, so no existing
  caller moves and the registry's claim becomes true — rather than renaming the
  declared type to match the accident, which would break a shipped path for a
  payload whose bytes do not change.

### Fixed

- **`sensor-pve`: a whole-job vzdump is one fact, not seven false criticals**
  (#880). The reference fleet's backup job is a single job covering every guest
  (`all 1`), so its tasks carry no per-guest id — PVE returns `id: ""` and the
  per-guest results live only in the task log. `text()` refuses an empty
  string, so every nightly task was silently discarded, and what survived the
  200-row window was whatever one-off task happened to be tagged with each
  vmid: for guest 120, a failure from six weeks earlier, reported as "the last
  backup" and firing a permanent critical about a dump that had in fact
  succeeded at 03:00 that morning. A template the job explicitly excludes fired
  too.

  Backups are now graded from the **stored volumes**, with the tasks as
  corroboration:

  - a whole-job run is kept as one job-scoped fact — new `state/pve/backup/job/{node}`
    document and `backup-job-failed` rule — instead of being attributed to
    guests PVE never named. Parsing the task log's free text for per-guest lines
    is a deliberate non-goal;
  - vzdump tasks have an age bound, `alerts.backup_task_max_age_secs`, default
    48 h. The task query is bounded by rows, not time, so without it the oldest
    surviving one-off wins forever;
  - a failed task that a **newer volume** supersedes no longer fires;
  - templates and `exempt_vmids` are skipped in the backup rules, as they always
    were in the guest rules;
  - `PveBackupSummary.volumes` is `Option<u32>`. `None` when no backup-capable
    pool could be listed at all; `0` now means only "this guest has no backups".

  Also fixed, and independent: `sweep()` **took** the backup cache while
  refilling it only every `backup_interval_secs`, so with the shipped 60 s/900 s
  cadences fourteen sweeps in fifteen carried an empty vec — no backup document
  published, no backup rule graded, and `reconcile` reading that as "the
  condition cleared". Every backup alert resolved and re-fired on a 15-minute
  cycle. The e2e missed it because it swept two *different* pollers.

- **`sensor-pve`: pool over-commitment is reportable on a `dir` storage** (#881).
  PVE surfaces per-volume sizes for LVM-thin and ZFS and nothing for a `dir`
  storage, and summing the empty set gave `Some(0)` — "nothing is provisioned",
  the exact opposite of the truth — so the rule this sensor leads with ("990 GB
  provisioned on a 937 GB pool") could never fire on the storage type the
  reference deployment actually runs. `PveStoragePool`'s own doc comment already
  said `None`, never zero.

  Where the plugin reports nothing, `allocated` is now **derived from the
  guests**: every disk line already names its storage and its declared size, and
  the guests are joined before the pool loop. New `allocated_source` field and
  gauge label distinguish `reported` from `derived_from_guests`, because a
  derived total is a **floor** — a detached `unused<N>` volume still occupies the
  pool and is deliberately not counted. On the reference fleet that yields
  790 GiB against 936 GiB, ratio 0.84, matching that fleet's own records.

- **`sensor-pve` says when the API refuses it** (#880). A 403/404/501 became
  `Ok(None)` with **no log line at any level**, and every caller logged failures
  at `debug` — so a token whose role is narrower than `PVEAuditor` produced
  `volumes: 0` and no `allocated`, in silence, at the shipped `logging.level:
  "info"`. Refusals and listing failures now warn once per endpoint per
  transition, and say what the consequence is.

### Added

- **`zensight-sensor-pve --diagnose`** (#880): a one-shot that asks the
  configured API everything the backup and storage rules depend on — which pools
  will be listed, what each content listing returns, which volids name no guest
  this sensor can read, how old each vzdump task is, and what the guest disks sum
  to per pool — prints it in plain sentences and exits. Read-only, and it never
  opens a Zenoh session: debugging a token should not join a fleet. Follows
  `--discover` in the SNMP sensor (#825).

- **pve, container and probe telemetry lands on a host card** (#883, #884,
  #885). Three sensors shipped in 0.13.0 filed every series under the *subject
  being described* rather than the *host doing the describing*: a VMID (`160`),
  a container name (`systemd-vaultwarden`), a probe target (`forge-http`). The
  GUI groups host cards by `(protocol, source)`, so on the reference fleet 438
  series fragmented across **41 identities that are not hosts** and the operator
  who opened the 0.13.0 GUI reported, correctly, that "pve, container and probe
  do not show any metrics".

  `source` is now the reporting host in all three, as in sysinfo. Nothing is
  lost: the subject was already in the key path (`guest/160/…`,
  `systemd-vaultwarden/…`, `forge-http/…`) and in rich labels
  (`vmid`/`name`/`node`, `container`/`unit`/`image`, `target`/`kind`/`vantage`),
  and `exposition.rs` sources the Prometheus/OTel per-subject dimensions from
  the key's pattern vars, not from `source` — so every existing dimension
  survives and the `source`, `host_name` and `hostname` labels become correct
  rather than naming a guest. The `device` identity model stays for things that
  really are separate devices, which is why snmp is unchanged.

  Three consequences worth naming:

  - **pve's alert `source`** moves to the host too. This re-keys nothing:
    `alert_key` hashes `rule` + labels and has never included `source`.
  - **probe alerts gained a `probe` label** (the operator's own name for the
    check), because an alert key is a digest and without it nothing on the
    alert said *which configured target* it was about once `source` became the
    vantage point. This does re-key probe alerts — and #882's adoption clears
    the old keys on the first restart, with no manual sweep.
  - **backup points gained a `vmid` label** and **cluster points a `node`
    label**. Both families previously carried no labels at all, so a consumer
    holding one as a value had no idea what it described.

- **`sensor-pve`: the API endpoint address is not an identity** (#885).
  `pve.source` fell back to `pve.host`, which on the deployment
  `configs/pve.json5` and `packaging/systemd/` both recommend — a native binary
  **on** the PVE node — is `127.0.0.1`. Every hypervisor-scoped series (pools,
  cluster, HA) was therefore filed under a loopback address, the one address
  guaranteed to be ambiguous across machines. It now falls back to the
  hostname, as every other host sensor does, which is also what makes this
  sensor's `evidence/self` agree with sysinfo's on the same box. The PVE node
  name rides as the `node` label, now on every series rather than some.

- **`sensor-container`: telemetry carries host identity** (#884). All 307 of
  307 points on the reference fleet carried none, while the same sensor's alerts
  carried `host.id`. Fixed by the above rather than by a new label: `source` is
  now the reporting host, which is exactly how sysinfo and snmp label
  provenance, so a `TelemetryPoint` handed to a consumer as a value knows where
  it came from. It also disambiguates the fleet — container names are unique per
  host, not globally, so four machines running `zensight-sensor-logs` used to
  produce four series agreeing on `source`, `metric` and every label.

  The gap that let all three ship: the e2e suites asserted only key
  expressions, never `point.source`, so they passed either way. All three now
  assert the reporting host on every point, and the subject in the labels.

- **Alerts are retracted, not abandoned — a firing set that outlives its
  process** (#882). When a condition cleared, `reconcile` published
  `Put(Resolved)` + a `Delete` tombstone, and always had. What no build did was
  survive its own restart: a new process starts with an empty firing set, so an
  alert that was firing beforehand and is no longer true is never fired again
  *and therefore never resolved*. Without a storage nobody noticed — the sample
  aged out of the network. The reference fleet deployed `zensight-latest` on
  `v1/*/state/**` on 2026-09-01 and the same afternoon watched three alerts sit
  `firing` for good: a `swap_thrash` still reading 2052 pages/s sixteen minutes
  after `si/so` went to zero, a `pressure_io` reading 50.9% against a live
  1.61%, and two `probe-down`s for targets that had been deleted from the
  config.

  Sensors now own both ends of their own lifetime, and `SensorRunner` drives
  both for any reporter handed to it with `with_alert_reporter`:

  - **on start**, `AlertReporter::adopt_persisted` GETs the producer's own alert
    selector and takes ownership of whatever the previous incarnation left
    there. Adopted alerts enter the active set already `published`, which is the
    truth, so the existing sweep finishes the job with no new lifecycle state:
    the first `reconcile` of each rule retracts what is no longer violated, and
    re-observing what still is publishes nothing. This is the half that covers
    SIGKILL, an OOM kill, and the common case of a restart whose config no
    longer defines the target.
  - **on stop**, `resolve_all` — written for this and wired to nothing until now
    — retracts and tombstones everything still firing, before the session
    closes. Three docs, `RELEASING.md` included, had claimed this already
    happened.

  Three shapes are retired on the spot rather than adopted, because no sweep can
  reach them: a `Resolved` whose `Delete` was lost, a document whose key does
  not match the `alert_key` its own payload derives (the #737 re-key stranding,
  which producers now clear for themselves — see `RELEASING.md`), and a rule the
  build no longer has, for producers that declare their rule table with
  `with_known_rules`. A deployment with no storage answers the GET with nothing
  and behaves exactly as before.

  Handing the runner the reporter now also declares `serve_alerts_query`, so one
  registration replaces three rituals; the eleven per-sensor
  `runner.spawn(serve_alerts_query(…))` lines are gone.

## [0.13.0] - 2026-08-31

**The fleet release** (epic #810). The sensors ZenSight shipped were excellent
at the machine as a *Linux host*: `/proc`, PSI, netlink, journald, D-Bus, the
wire. The reference deployment is not primarily a Linux host — it is a
**Proxmox hypervisor running six VMs whose entire workload is Quadlet
containers, reached from outside over TLS** — and of those four nouns ZenSight
understood one.

Every finding of that fleet's 2026-08-28 audit was something a sensor could
have been asserting continuously, found instead by a human reading
configuration carefully, once, weeks late. Three new sensors turn those
findings into assertions:

| Found by hand | Now asserted |
|---|---|
| VM 140 had `onboot=0` and `firewall=0` on its NIC — its firewall file was inert and :8000 was open to the whole service zone | `pve` |
| 990 GB provisioned on a 937 GB pool | `pve` |
| garage reporting `unhealthy` since deployment while working fine | `container` |
| cosign silently signing nothing for eight days | `container` |
| 12 pinned images behind upstream, surfaced by a monthly mail | `container` |
| the `/etc/hosts` hairpin, diagnosed after eight days by noticing a 20 s connect timeout | `probe` |

#### Upgrading to 0.13.0

**Nothing breaks.** No wire contract moved, no series was renamed, and no
default changed for an existing sensor. The three new sensors are additive and
none of them starts on its own: `pve` needs an endpoint and a read-only
`PVEAuditor` token, `container` needs a runtime socket, and `probe` needs
targets. None is in `just run` or the all-in-one demo bundle; each ships as a
per-sensor image and a hardened systemd unit, per #813.

**Read before deploying `pve` or `container`:** neither has met a real Proxmox
API or a real podman socket. Both are tested against in-process fakes built
from the documented API shapes and from the audit's own failures — an LXC NIC
line whose MAC lives in `hwaddr`, a `firewall` key that is absent rather than
`0`, a healthcheck reporting `unhealthy` with an empty log. The first
deployment of either is the first real test, and should be read as one.

**One thing that is opt-in on purpose:** `container`'s upstream-digest and
cosign-signature checks are the only part of any of these sensors that leaves
the host. They are off by default and refuse to start with an empty registry
allowlist.

### Added

- **`zensight-sensor-pve` — the hypervisor as a hypervisor** (#818). The
  reference fleet's Proxmox host was watched by three native binaries
  reporting CPU, memory, disks, units and the journal: a complete picture of a
  *Linux box*, on the one machine whose failure is total. Everything that made
  it a hypervisor was invisible, and the 2026-08-28 audit found three things by
  hand, once, weeks late — VM 140 with `onboot=0` (it would not have come back
  after a host reboot), that guest's NIC with no `firewall=1` (so `140.fw` was
  inert and :8000 was open to the whole service zone for an unknown period),
  and 990 GB provisioned on a 937 GB pool. **None of those is a metric that
  spikes**; they are configuration facts that stopped matching intent, and all
  three are now continuous assertions.

  The sensor polls `/api2/json` with a read-only `PVEAuditor` token (through
  the framework's `file:`/`${ENV}` indirection, so the secret never enters a
  config file) and publishes: per-guest state documents joining runtime status
  with the config that decides the *next* reboot (`onboot`, per-NIC
  `firewall`, per-disk `backup=0`, provisioned size); per-pool capacity, use
  and **allocated** — the promised total that is invisible in `used` and fills
  a thin pool on its own schedule; per-guest backup summaries carrying the
  newest two volumes, so **a dump that succeeds while halving** is expressible
  where a green exit code is not; cluster quorum, HA and replication; and a
  third-party identity claim per guest (name + configured MACs) so the
  hypervisor's view of a VM fuses with that VM's own sensors in the catalog.
  Ten alert rules, each reconciled every sweep.

  **There is no action surface — not disabled, absent.** Nothing in the crate
  constructs a non-GET request, the registry slice declares no `write`
  procedure, and a test fails if one ever appears: a monitor that can stop a VM
  is a different threat model and would have to be a separate, deliberate
  decision. Three poll cadences (status 60 s, guest config 300 s, backups
  900 s), a concurrency cap, and a startup that refuses a timeout not shorter
  than its interval, because the API is a perl daemon on that same machine. A
  403 is treated as a fact about the install (a read-only token, or an install
  without HA), never as sensor failure.

  Not in `just run`: no demo can invent a Proxmox endpoint or a credential.
  `just pve` runs it, `packaging/systemd/` ships a hardened unit for the
  native-on-the-hypervisor pattern the reference deployment's security rules
  require, and `packaging/quadlet/` covers polling from a guest. Tested against
  an in-process fake API serving Proxmox's real document shapes — including its
  inconsistencies, which is where the bugs were: an LXC NIC line carries its
  MAC in `hwaddr` rather than positionally, and `firewall` absent means *off*
  while `backup` absent means *on*.

- **`zensight-sensor-container` — the whole workload, previously invisible**
  (#819). Every service on the reference fleet is a Podman Quadlet container,
  and no sensor knew what a container *was*: sysinfo's cgroups collector is
  off by default and cannot enumerate, the systemd sensor sees `caddy.service`
  as a unit rather than as Caddy 2.11.4 at a particular digest, and netlink
  surfaces the podman bridges' containers as eleven catalog rows with IPs and
  nothing else. Four separate findings of the 2026-08-28 audit are now fields.

  The sensor joins two sources. From the **runtime socket** (read-only,
  podman's libpod API or Docker's compatibility API): image reference **and
  digest**, healthcheck state, restart count, last exit code, ports, mounts,
  restart policy, and — through the `PODMAN_SYSTEMD_UNIT` label — the systemd
  unit that owns the container, which every alert carries so an operator gets
  something restartable instead of a container id. From the **kernel** (cgroup
  v2): `memory.current`, `memory.max`, `memory.peak`, CPU time and throttling,
  `oom_kill`, PSI and pids — the per-container numbers whose absence made the
  2026-08-17 OOM "the bundle" for eleven days.

  The distinction that matters most: **`unhealthy` and "the healthcheck has
  never produced a result" are different facts**, and they had been rendering
  as the same one. garage reported `unhealthy` from the day it was deployed
  while serving traffic perfectly — a distroless image with no `/bin/sh`, so a
  `CMD-SHELL` probe could never execute — and nobody noticed for weeks,
  because the alert sent people to debug garage. `HealthState::NeverRan` is a
  separate rule with a separate sentence, and the `healthy` gauge is **not
  published at all** in that state: a `0` there tells every dashboard the
  service is down.

  Seven rules. The restart-loop and OOM rules grade the **delta** against the
  previous sweep, because both counters are cumulative and firing on the total
  would alert forever about a kill from last year.

  **Read-only, and no egress by default.** The socket client has two methods
  and both are GETs; the registry slice declares no `write` procedure and a
  test fails if one appears; the shipped units mount the socket `:ro` anyway.
  Exactly one collector leaves the host — the upstream-digest and cosign
  signature checks, which replace `image-update-report.sh` and its monthly
  mail — and it is off by default, restricted to a **named registry
  allowlist** that startup refuses to leave empty, and anonymous (no
  credentials are read, sent, or stored). "Not checked" is never reported as
  "behind" or "unsigned": silence is not evidence.

  Not in `just run` — on a host with no runtime it would report a failure
  every cycle for something that host does not do — and not in the demo
  bundle; `just container`, a hardened systemd unit and a quadlet that mounts
  the socket and cgroupfs read-only. Tested against a real UNIX-socket HTTP
  server serving real libpod documents and a real cgroup tree on disk, with
  the fixture built from the audit's own failures.

- **`zensight-sensor-probe` — the outside-in view** (#820). Everything else
  ZenSight measures is *inside*; nothing checked that the thing works from
  outside. That gap cost eight days: on 2026-08-20 a reboot dropped an
  `/etc/hosts` entry, a guest resolved `git.marcpardo.eu` to the public IP it
  cannot reach (the edge DNAT matches the external interface only), cosign and
  Renovate both broke, and the diagnosis eventually hinged on someone noticing
  that failing CI runs took *2m16s* — a 20 s connect timeout — and that the
  forge's router log showed zero requests. Two issues were filed on an
  expired-token theory first. A guest-side probe would have said "timeout,
  20 s" within one interval.

  Six check kinds: **HTTP** (status, expected-status and body match, TTFB,
  redirect chain), **TLS** (chain validity, days to expiry, issuer, SANs and
  SAN match, protocol), **DNS** (answers, and **the resolver named** — without
  which "resolves to the wrong address *here*" cannot be written down),
  **TCP**, opt-in **ICMP** behind an `icmp` build feature, and **local
  certificate files**, which need no network at all and retire the monthly cron
  that watched ZenSight's own mesh certificates.

  Three deliberate distinctions. **A timeout is its own outcome**, not a
  failure with different text: `probe-timeout` suppresses the generic
  `probe-down`, and the duration rides on the alert, because on 2026-08-20 the
  duration *was* the diagnosis. **The vantage point is half the answer** — the
  same target from the edge, from a guest and from a workstation gives three
  different, equally true results, so `vantage` is on every document and every
  alert and two hosts disagreeing is the finding rather than a contradiction.
  **An absent verdict is not a negative one**: a PEM on disk has no chain, so
  `chain_valid` is `None` and no gauge claims otherwise, and a check that did
  not run says so in words that deny being evidence about its target.

  Bounded by construction and checked at startup: an explicit target list, a
  5 s interval floor so it cannot be configured into a load generator, a
  concurrency cap, unique target names, per-kind target shapes, and a timeout
  that **must** be shorter than its own interval. A client only — no listeners,
  no write surface, and a test that fails if a `write` procedure ever appears
  in the slice.

  Joins the CI conformance roster: with an empty target list it reaches nothing
  at all, declares its slice, serves it, and is judged like any other producer
  — verified with six producers live and zero gated findings. It is **not** in
  `just run`: every example target ships commented out, because no generator
  can invent a URL worth watching. The docs say plainly what it does not do —
  *a probe running on the server cannot tell you the server is unreachable* —
  and the sensor logs that at startup.

- **SNMP: a per-device PDU budget, and a one-shot `--discover`** (#825 items 2
  and 4 — items 1 and 3 shipped in 0.12.0, and the issue closes with these).

  **The budget.** An SNMP sensor's characteristic failure is hammering a device
  weaker than itself — an eight-year-old switch CPU, or a UPS management card
  that reboots under load. Until now each device polled on its own timer with
  no cap on outstanding requests and no ceiling on PDU rate: correct, and
  entirely dependent on the operator having chosen a gentle interval. Devices
  gain `max_pdus_per_sec` and `max_concurrent`; both absent means no ceiling,
  so every existing deployment behaves exactly as before. This is the
  SNMP-shaped instance of #812's fleet-wide budget work, and it is declared
  **per device** because the resource being bounded is *someone else's device*
  and one switch's tolerance says nothing about another's.

  The accounting is honest about what it can know: a GET costs one token,
  charged before it is issued; a **walk is charged after it completes**, from
  the rows it really returned (`ceil(rows / max_repetitions) + 1` for GETBULK,
  `rows + 1` for GETNEXT on v1). How many PDUs a walk takes is not knowable
  before the table is read, and estimating it would make `max_pdus_per_sec` a
  number meaning something other than what it says. A large table therefore
  drains the bucket and delays the *next* operation — the device gets a rest
  proportional to the work it just did. Over budget the poller **waits**; it
  never drops a poll, because a sensor that skips work to stay under budget has
  traded the device's health for a gap in its own telemetry. Pinned by an e2e
  that measures the wall clock against a live agent under a 10 PDU/s ceiling
  and then asserts all 64 rows still arrived. (GETBULK itself is not new — the
  client has picked it for v2c/v3 since #559, pinned by `v2c_walk_uses_getbulk`.)

  **`--discover <cidr>`.** The gap between "supported" and "usable" for SNMP is
  always the config. The new one-shot mode sweeps a subnet, identifies what
  answers by sysName/sysObjectID/sysDescr, prints a **proposed config to
  stdout**, and exits — annotated per device, with a header saying that nothing
  was applied, that the name becomes the device slug in every key, that a
  device answering a community answered a *cleartext* credential and needs
  `allow_insecure_versions`, and that anything smaller than the machine polling
  it wants a `max_pdus_per_sec`. It **never touches the bus**: the runner, the
  session and the publishers are not constructed at all on that path, so an
  operator sweeping a subnet from a laptop does not thereby join a fleet.
  Diagnostics go to stderr, so `--discover 10.0.0.0/24 > devices.json5` yields
  a file that is only the proposal. Addresses already in `snmp.devices` are
  skipped, and an empty sweep prints a sentence rather than an empty file —
  *silence from an SNMP agent is indistinguishable from silence from a filtered
  port* — and exits 0, because that is a finding rather than a failure of the
  sweep. It is distinct from the existing `snmp.discovery` block, which is a
  continuous in-process sweep publishing a `DiscoveryReport`: the two answer
  "what is out there right now, so I can write a config" and "what appeared on
  my network since I last looked".

- **Gated systemd service control can finally be demonstrated** (#866). The
  whole surface — allowlist matching, the arm/confirm/cancel flow, the
  in-flight lock, the audit ring on `@rpc/systemd/actions`, c620838's
  refuse-don't-hide contract — shipped correct end to end and **permanently
  un-demonstrable**: `actions.enabled` has been false in every generated config
  since the block existed, and there was no opt-in path at all. That is the
  blind spot #845 closed for the exporters, one feature over. Default-off stays
  (it mutates real units and needs polkit), but there is now a lever:
  `sudo scripts/demo-actions.sh install` creates `zensight-demo.service` — an
  inert `sleep infinity` under `DynamicUser` with no network,
  `ProtectSystem=strict` and an empty `CapabilityBoundingSet` — plus a polkit
  rule granting `manage-units` for **that unit, to that user, and nothing else**
  (no `manage-unit-files`, no `reload-daemon`); `just actions=1 run` then arms
  the sensor for exactly that glob via the new `gen-configs.sh --actions UNIT`.
  `sudo scripts/demo-actions.sh remove` puts the machine back. The demo never
  touches a unit anything depends on, and the root requirement is asked for
  explicitly rather than hidden inside a build recipe.

- **A refused action says which switch refused it** (#866). `ActionCapability`
  gains an optional `reason` (additive; older sensors omit it, older frontends
  ignore it), served with the `enabled: false` answer the sensor already gave.
  It distinguishes two facts that looked identical from outside — the master
  switch being off (naming `configs/systemd.json5`) and an empty `allow_units`
  refusing every unit — and the GUI shows the host's words in place of its own
  generic sentence, which stays as the fallback. Only the sensor knows which
  file holds the switch; a gated control that cannot say why it is gated reads
  as a broken one.

### Fixed

- **hostspec no longer demos as a blank pane** (#867). Nothing was broken —
  the shipped assertion set is empty on purpose, `gen-configs.sh` copied the
  file verbatim, and the sensor said so in its logs — but *an empty pane is
  indistinguishable from a broken sensor*, which is why it was reported as a
  regression by the person who had built it two days earlier. Two independent
  halves: (1) `configs/hostspec.json5` gains a `//DEMO `-marked block that
  `gen-configs.sh` uncomments for `demo-max` and leaves alone for
  `production`, so the shipped default stays the documented empty set while
  `just run` asserts two things true on any Linux host (`/` mounted,
  `/etc/hostname` exists) and one that **deliberately fails** — a required
  listener on `:65001` — giving the demo both a green sweep and a real firing
  alert; the netlink `demo-expected-service` motif, inverted only in which
  profile edits. (2) The Expectations view now distinguishes "not fetched
  yet" from "held to nothing": once the sensor has answered, an empty set
  reads as the state it is, with the `@rpc/hostspec/spec` answer shown
  verbatim beside it. Pinned by a config test that applies the generator's own
  transform (the demo block would otherwise rot unnoticed while commented out)
  and two UI tests, one per fact.
## [0.12.0] - 2026-08-31

**The operator release** (epic #809). ZenSight has been a very good instrument
and a very poor operator: it measured more of its reference fleet, more
precisely, than anything else running there — and in three weeks of production
it never told anybody anything, while the sensor bundle OOM-killed a 1 GB VM
on 2026-08-17 *while reporting `status: Healthy`*. Both halves of that were the
same missing idea: **no model of itself as a thing that runs somewhere and must
behave.** This release adds it, in four parts.

- **Self-knowledge** — a sensor can now see its own size (#811: RSS, VSZ, CPU,
  cgroup context and per-table occupancy in the health doc) and act on it
  (#812: a declared budget and a four-step shed ladder that evicts, degrades
  and saturates instead of dying). #814 is the first consumer: every netring
  table is bounded in bytes and `production` became a sizing profile.
- **Reach** — every state family's served schema is a gated contract (#815),
  which is the precondition for a notifier that can leave the bus.
- **Blast radius** — per-sensor packaging (#813): one `.container` per sensor
  with its own `MemoryMax` and its own on/off switch, so one sensor's death
  stops taking the other four with it during an incident.
- **Control** — fleet configuration as desired state (#816): the `@desired`
  service slice, an applied-config marker that says who won, and hostspec
  (#821) as its first reconciling citizen — a read-only sensor that holds a
  host to machine-checked assertions and executes nothing.

One entry below breaks a *deployment*: the SNMP sensor now refuses to start on
a v1/v2c config without an explicit flag. Read **Changed — BREAKING** first.

#### Upgrading to 0.12.0

1. **SNMP configs using v1/v2c will refuse to start.** Either move the device
   to v3 authPriv (the shipped example now shows it) or set
   `snmp.allow_insecure_versions: true` and accept the documented cost. The
   refusal names the device and the flag.
2. **No alert re-key this time** — the `RELEASING.md` sweep does not apply to
   this release.
3. Optional but recommended: give each sensor a budget
   (`resources.budget_rss_mb`, or a cgroup `memory.max` the governor can
   discover) so the shed ladder can arm, and adopt `packaging/quadlet/`'s
   per-sensor units in place of the all-in-one bundle.

### Changed — BREAKING

- **SNMP leads with v3; the cleartext versions are now an explicit opt-in**
  (#825 items 1+3). `configs/snmp.json5`'s example device is SNMPv3 authPriv
  (SHA256/AES, credentials through the existing `file:` indirection), because
  in 2026 that is the shipped default and not the aspiration. v1/v2c moved
  behind `snmp.allow_insecure_versions` (default **false**): `validate()`
  refuses to start with any v1/v2c device — or any trap-listener community —
  configured, naming the device and the flag. A community string is a
  cleartext credential on the wire, and netring ships a detector for exactly
  that; accepting the cost should be something someone typed, not something
  someone copied. **Breaking for any config that relied on the old v2c
  example**: set `allow_insecure_versions: true` (the commented legacy-device
  example shows the opt-in beside its cost), or move the device to v3. The
  refusal message says which.

  Item 3 of the same issue was *verified* rather than built, and pinned: traps
  already ride the `events` class as durable `EventRecord`s, with alerts only
  through the bounded rule mapping. `zensight-sensor-snmp/tests/trap_storm.rs`
  now proves a storm cannot amplify — 50 identical traps are one alert key, one
  bus publication, `active_count` 1 — with the one deliberate exception (a
  severity escalation re-publishes; a worsening alert must not be muted by
  idempotence) pinned beside it. Items 2 (per-device PDU budget) and 4
  (discovery mode) remain open on #825.

### Added

- **hostspec reconciles `@desired`** (#816, closing PR): the assertion set
  is the fleet's first desired-authorable topic. The sensor wires
  `reconcile_topic` beside its RPC surface — three writers (file baseline,
  desired, RPC) onto one sentinel handle, one shared
  `applied/expectations` marker saying who won last (the RPC path now
  stamps `source: rpc`), the same `validate()` gating all of them. The
  deployment storage config gains a `zensight-desired` storage (own
  selector — D4; week-long GC lifespan, because the storage's seed-GET
  answer is the reconciler's primary convergence path and a collected doc
  would silently un-configure a returning sensor). systemd/netlink/logs
  join via #849.

- **The `@desired` reconciler** (#816 pt 2):
  `zensight_sensor_core::desired::reconcile_topic` — seed GET + periodic
  re-GET as the level-triggered primary path (survives missed samples,
  reconnects, router restarts) with the AdvancedSubscriber
  history+recovery recipe as the accelerator; LWW by sample timestamp
  (unstamped refused — unorderable); invalid documents rejected loudly
  with the previous good config kept and the rejection riding the applied
  marker; Delete reverts to the file baseline; the kill switch declares
  and applies nothing but still publishes `source: file` ("disabled" must
  never read as "silent"). E2E over an isolated pair pins late-start
  convergence through a publisher cache, both rejection paths, the
  delete-revert, the kill switch, and the zenoh-ext verbatim-chunk canary.

- **The `@desired` service slice + the applied-config marker** (#816 pt 1).
  New registry slice `desired.toml`: a controller publishes per-host runtime
  policy under `v1/@desired/state/<host>/<producer>/<topic>` (target host
  first — RFC 07 §3's G1 rule, H4-linted), LWW, storage-backed, ttl 0
  (policy never ages out; delete reverts to file baseline). v1 carries the
  hostspec topic; the other sentinels join when their config types gain real
  schemas (#849 — the #815 gate refused the summary stubs, correctly, which
  also drove hostspec's wire types into `zensight-common/src/hostspec.rs`
  as real schemars types, renaming the registry type to
  `HostspecExpectations` via a pre-release force-relock). New shared types
  in `zensight-common/src/desired.rs`: `DesiredConfig` (file-config kill
  switch + refresh cadence) and `AppliedConfig` — the
  `state/<producer>/applied/<topic>` marker saying which source won last
  (`file|desired|rpc`), the doc in force (JSON-encoded — its schema is the
  topic type's own), and the last **rejected** desired doc, so a refusal is
  on the bus, not only in a log. KEYSPACE.md documents the contract,
  including the never-list: nothing under `@desired` may carry secrets or
  bus-reachability config.

- **hostspec assertions are authorable from the GUI** (#821, closing PR):
  the Expectations view gains the `hostspec` target — eight authoring kinds
  (require/forbid listeners split), whole-set push of the plain
  `ExpectationsConfig` (sensor-side validation; a refusal keeps the
  previous set), rule-slug rows shared with the sensor's alert rules so
  Remove removes the thing that is firing, and the #791 verdict chip on
  the status reply. With this, #821 is done: the sensor, its registry
  slice, fleet/CI integration, and the authoring surface.

- **hostspec is a fleet citizen** (#821, integration PR): in the conformance
  CI roster (it is the ideal CI sensor — no privileges, no devices, empty
  default set, every procedure served), `just sensors`/`run-sensors.sh`,
  gen-configs, both sensors images, all four release lists, and a hardened
  systemd unit that is the least privileged in packaging/ (DynamicUser,
  ProtectSystem=strict, empty capability set; ProtectHome=read-only so
  /home assertions stay observable). Docs tables and counts updated
  throughout; KEYSPACE's fleet-push list gains hostspec expectations.

- **`zensight-sensor-hostspec`** (#821): machine-checked desired-state
  assertions — the sentinel pattern for what D-Bus and netlink cannot see.
  Seven read-only assertion kinds (mounts incl. bind-of through computed
  device+root, files with mtime freshness and a latched size baseline, TCP
  listeners with require/forbid and distinct `0.0.0.0`/`::` wildcards,
  literal symlink targets, provable absence, capped content
  contains/matches, permissions incl. lstat-only secrets) — a CLOSED
  vocabulary that **executes nothing** (no command/run kind ever; the
  binary/version kind was deliberately rejected). Failures are alerts with
  the failing clause in the labels; the set hot-swaps over
  `@rpc/hostspec/expectations/set` with real validation before apply
  (refuses `error/invalid-args`, previous good set kept — new over both
  older sentinels); `@rpc/hostspec/spec` answers "what is this host held
  to" per-assertion as pass/fail/**unreadable** (an observation the sensor
  could not make is never a pass). One gauge `assertions/failing` publishes
  every sweep, 0 included. New registry slice `hostspec.toml` (registry
  1.0, lock regenerated); the shipped config's default set is empty on
  purpose — verified live: a 5-producer conformance run (sysinfo + logs +
  systemd + hostspec + catalog) passes strict with the slice in sync.

- **`Protocol::Hostspec`** (#821, plumbing PR): the enum variant, wire token
  `hostspec`, and the compiler-forced GUI arms (generic icon, generic
  overview, no specialized tab — its surfaces are Alerts, the Sensors card
  and, later in the epic, the Expectations form). The variant lands before
  the crate on the `Opcua` precedent, so the sensor PR stays crate-scoped.

- **The three payload verdicts, rendered** (#791 — with it, epic #726 is
  done). `zensight` gains a default-on `validate` feature enabling
  `zensight-common/validate-json`; the bus explorer's inspector judges the
  selected key's retained bytes against its declared type's schema, and the
  five fleet-RPC status panels (netlink/systemd expectations, netring
  detectors/capture filter/threat intel) judge their reply bodies at
  receive, via the generated `ProcedureId::reply_type()`. One chip renders
  all of it (`view/components/verdict.rs`, on `kit::badge` — meaning never
  by colour alone): `Valid` green, `Invalid` red with violations listed,
  and the six `NotValidated` reasons in two visual groups — *chose not to*
  (`FeatureOff`/`NoRegistry`, grey) and *could not* (`NoSchema`/
  `KindUnsupported`/`Undecodable`/`BadSchema`, the #746
  `JUDGEMENT_UNOBSERVABLE` violet). **"I did not check" never renders as a
  pass** — pinned by a property test, and the `--no-default-features` build
  (now a named CI features step) degrades to an honest "not checked". A
  tombstone or unregistered key gets no chip at all: absent, not judged.

- **Bus explorer** (#748, the last #726 child): a new "Bus" view — the live
  key-tree on `zenkey_fleet::Monitor`, bounded with explicit drop
  accounting. A pump task owns the monitor on the GUI's session (borrowed,
  never a second session) and folds per-sample work off the GUI thread; the
  view receives one snapshot per 250 ms stats tick whatever the bus rate.
  Lazy by construction (liveliness only — both D4 sweeps, so "catalog dead"
  and "no entities" render differently — until the user adds a watch), and
  every bound has its own ledger tile: keys 10 000 with an eviction count,
  broadcast shed, unwatched retirements, retention 16 MiB/60 s — four
  distinct loss facts, never summed (RFC 13), zeros rendered. Per-sample
  observed-vs-declared QoS (`qos_matches` against the generated registry's
  profile) surfaces `QosObservedMismatch` live, with unregistered keys as a
  distinct mark, never a mismatch. The inspector pane is the GUI's first
  payload-inspection surface — key, declared type, true size,
  declared-vs-observed axes, stamp provenance, bounded byte preview, scoped
  "latest retained sample, watched keys only" — the surface #791's verdicts
  will hang from. Demo mode runs the same session-free pipeline against a
  real `MonitorCore`, which doubles as proof the #747 replay seam drives
  this view deterministically.

- **`.zrec` captures as GUI decode fixtures** (#747, the fifth of six #726
  children). `zensight::replay` loads a zenkey-fleet tape capture and feeds
  it to the real decode path with no bus and no live time: `decode_row`
  (tombstones routed, the header's base stripped the way a session namespace
  would), `messages()` for an `App::update` fold, and `sample_view()` to
  lift rows into `MonitorCore::ingest_at`-shaped consumers (the #748 seam).
  The checked-in corpus in `zensight/tests/fixtures/zrec/` — sysinfo
  telemetry, the state plane, and the `@catalog` entity a `v1/*` selector
  can never see (grammar D4) — was recorded from a real isolated deployment
  by the new `scripts/record-fixtures.sh`, via the new (non-judging)
  `zensight-conformance --record-zrec` mode. Tests assert capture-stable
  facts only, so the regeneration contract is: re-run the script, the suite
  passes unchanged. `"bytes"`-is-the-payload is pinned by a lossless
  writer→reader round trip; determinism by folding a capture twice from two
  fresh boots. Synth/fault injection deliberately deferred (it sits behind
  the fleet `decode` feature the GUI keeps off).

- **Every state family's served schema is now a gated contract** (#815,
  unblocks zenkey#388's zenwatch). The audit found the invariant already
  holds — all 14 state-family types serve real schemars-derived schemas —
  so the gate pins it where the upstream checks cannot: `describe-totality`
  is name-presence-only (a stub passes) and `describe-missing` is Info by
  design. A test-time gate (the §6.1 subject-half precedent) asserts every
  `class = "state"` subject serves a generated schema with structure in
  every property, and pins the renderer-read fields of
  Alert/HealthSnapshot/ErrorReport/SensorInfo/HostEvidence/HostEntity by
  name. The one real hole is closed: `SensorInfo.metadata` rendered as an
  anything-goes schema and is now typed as an object. The CI conformance
  deployment widens from sysinfo-only to **sysinfo + logs + systemd** (both
  degrade rather than exit on a journal-less/bus-less host), so served
  schemas, seeds and payloads of four producers are live-judged every run.

- **sysinfo: a `smart` collector — the drives themselves, before mdadm
  reports the aftermath** (#823). Default off. NVMe health via the admin
  health-log ioctl (wear `percentage_used`, `available_spare` vs threshold,
  `critical_warning` bits, media errors, power-on hours, unsafe shutdowns,
  data units) and the three classic ATA attributes (reallocated/pending
  sectors, UDMA CRC errors) via SG_IO pass-through — kernel interfaces, no
  smartctl. Ioctls run every 60s off-thread; missing devices or permission
  (CAP_SYS_ADMIN / CAP_SYS_RAWIO — see the unit-file notes) skip silently
  per arm; NVMe temperature stays with the `temperatures` collector (kernel
  nvme hwmon). Twelve new registered `smart/{device}/*` families (registry
  1.4). Four alert rules: `smart_spare` (spare at/below its own threshold,
  Critical), `smart_critical_warning` (any bit, Critical, bits spelled
  out), `smart_media_errors` (per-device delta, Warning),
  `smart_sata_attrs` (newly reallocated or pending sectors, Warning).
- **netring: every table bounded in bytes, and `production` is now a sizing
  profile** (#814). The 319 MB-on-a-quiet-scanner class of growth is
  structurally gone: the eight refuse-at-cap L7 inventories become
  byte-capped true-LRU `BoundedTable`s (five `tables.*` knobs; a full table
  now admits new data by evicting stale data — the freeze-stale and
  asset-desync bugs retire together), `HttpPending` gets a TTL sweep, and
  the two genuinely unbounded detector structures finally have their
  upstream eviction hooks driven on event time (no forks). Every bounded
  table registers with the #812 governor — occupancy/caps in the health
  doc, one shared evict path for local overflow and budget pressure — and
  `anomaly_detectors` is the registered degradable. `production` now also
  *sizes*: 64 MiB budget, passive-DNS at 2048 IPs, inventory budgets
  halved — ~4.5 MiB of hard table caps, visible in config rather than
  emergent; `demo-max` keeps the full envelope.

- **sensor-core: a memory governor, and a sensor that sheds instead of
  dying** (#812). The health tick now drives a shed ladder against the
  declared budget — Evict (LRU from the largest registered table, then
  `malloc_trim`) → Degrade (registered optional work stopped, health status
  `Degraded`) → Saturated (the loudest possible report) — and **dying is not
  on it**. Budgets are config-declared or, in a container, discovered as
  75% of cgroup `memory.max`, so the ladder cannot disagree with the
  operator's drop-in; thresholds agree with the `sensor-budget` alert
  (80/95/75). The ladder's state (`self_stats.ladder`: step, per-table
  evictions, degraded list, a human reason) publishes every tick — a
  silently degraded sensor is a lying sensor. Sensors register evictable
  tables and degradables on `runner.governor()`; `with_alert_reporter`
  routes the runner's budget alerts through the sensor's own seeded
  reporter (retiring the #811 seed gap). No budget + no cgroup limit = the
  ladder never arms; nothing changes for unwired sensors.

- **HealthSnapshot: a sensor that can see itself** (#811). The health doc
  gains an optional `self_stats` block: self-measured RSS/VSZ/CPU
  (`/proc/self`, on the 5s health tick), the declared budget, publish
  counters (every baseline put counted in messages and bytes; sensor-fed
  dropped/evicted), per-table occupancy from registered providers, and the
  sensor's own cgroup-v2 memory context. Every field optional and
  serde-defaulted — absent is *not measured*, never zero; no registry
  changes (additive type evolution on the existing `health` subject). A
  declared budget (`SensorConfig::budget_bytes()`; netring's
  `resources.budget_rss_mb` is the exemplar, with flow-ring/TLS/asset table
  providers wired) arms the **`sensor-budget`** rule: Warning ≥80%,
  Critical ≥95%, release <75%, message naming the largest table. The GUI
  sensor cards show RSS and budget% when present. The 2026-08-17 incident —
  110→355 MB on a 1 GB VM, `Healthy` throughout, found eleven days later by
  hand — is now visible in the health doc it was invisible in. Enforcement
  (the shed ladder) stays #812.
- **logs: kernel pattern built-ins, per-rule rate limits, and redacted
  quoting** (#824). The sentinel gains `include_kernel_builtins` (off by
  default): `ext4-fs-error`, `xfs-corruption`, `md-raid-failure`,
  `block-io-error` — Critical, rate-limited, source-agnostic. Every rule
  takes an optional `rate_limit: { max_fires, per_secs }` capping alert
  publications (suppressions counted and surfaced in `@rpc/logs/rules`). A
  quoted line is scrubbed before it leaves the host — secret-looking
  `key=value` assignments are replaced in `{message}` and capture groups, and
  a scrubbed summary is suffixed `(redacted)`.
- **systemd: a timer's *service outcome* is now judged, not just its
  schedule** (#824). Timer expectations gain `succeeded_within_secs` — the
  timer fired within the window **and** its triggered unit's last run
  succeeded (`Timer.Unit` → `Service.Result`; unreadable = not satisfied,
  never "passed") — under the new `expect-timer-succeeded` rule. New
  threshold rule `systemd-consecutive-failures` (default 3, 0 disables)
  counts consecutive failed *runs* by `InvocationID` change, which is
  `restart_storm`'s logic applied where `NRestarts` cannot see: a
  timer-triggered oneshot never restarts. The GUI expectations editor
  round-trips the new form ("Timer service must succeed within").
  *Breaking (config/API): `TimerExpectation.within_secs` is now optional —
  JSON5 configs and `expectations/set` payloads are unaffected unless they
  omitted it, which was never valid.*
- **sysinfo: per-mount disk/inode threshold overrides and a `disk_fill_rate`
  rule** (#822). `alerts.disk`/`alerts.inode` take a `mounts` list (exact or
  glob path, first match wins, per-field fallback) so `/` can warn at 75%
  while the build-scratch volume warns at 92%. The new `disk_fill_rate` rule
  fits a least-squares trend over the recent `used_bytes` history and alerts
  on **projected time-to-full** (default: Warning ≤24h, Critical ≤4h, 30-min
  window) — silent until enough history exists, silent on a flat/shrinking
  disk, and reset by a large reclaim, so absence always reads as *not asked*.
  On by default; no registry changes (alert rules ride `alert/{alert_key}`).

### Changed

- **Per-sensor is the fleet unit; the bundle is the demo** (#813). The
  all-in-one sensors image gave five sensors one cgroup and one `MemoryMax`:
  on vm-edge, 2026-08-17, netring grew, the kernel picked a victim, and
  `FAIL_FAST` handed the victim's exit to the entrypoint — which killed
  sysinfo, netlink, logs and systemd on its way out. The four sensors that
  would have explained the incident were terminated by the one that caused it,
  and "disable netring *there*" was not expressible. `packaging/quadlet/` now
  ships one `.container` per sensor against the per-component images every
  release already builds — each with its own `MemoryMax` (reference-fleet
  starting points; measure with the health doc's `self_stats`), its own
  `Restart=on-failure` and its own on/off switch — and `docs/DEPLOYMENT.md`
  demotes the bundle to the one-command demo it is. The demo the bundle keeps
  is fixed too: `ZENSIGHT_SENSORS=sysinfo,systemd,logs` subsetting in
  `run-sensors.sh` and through the container entrypoint (the missing lever),
  and `FAIL_FAST` is gone, replaced by per-child supervision — exponential
  backoff (2, 4, 8 … 60 s), restarts logged, and a child that spends its
  `MAX_RESTARTS` budget given up on while the rest keep running.

- **The OTLP exporter is executed in CI** (#845, finding 15 — the medium
  one). demo-verify.sh gains a phase 2: the otel exporter joins the same
  isolated hub the Prometheus phase stands up, pointed over OTLP/HTTP at a
  stdlib-python sink, and the gate is that a real metrics export ARRIVES —
  protobuf content-type, non-empty body. Until now the exporter's only
  executions were `--help` in the release smoke and a manual `just
  demo-otel`; the exact gap (#752/#753) that demo-smoke closed for
  Prometheus had been standing open on the OTel side since the crate landed.

- **CI stopped being happy** (#845, 14 evidenced leniency fixes). The
  conformance gate's one exclusion (`field-new`) was stale — its lift
  condition, zenkey#384, had shipped in the pinned fleet 0.11.1 — and is
  lifted; a 660-sample live run confirms zero findings, and the README now
  requires any future exclusion to carry a re-check for its own
  lift-condition. Conformance runs `--strict-window` (a listen window that
  shed samples is unobservable, not quietly clean). `zensight-common` is now
  also tested ALONE, so the #791 FeatureOff contract tests actually compile
  in CI. The features job runs `clippy -D warnings` instead of `check`
  (warnings could land silently in every feature-gated path) and gains
  `--no-default-features` legs for logs (journald-less) and zensight-btf
  (no_std). Both awk test-strippers now suppress a `#[cfg(test)]` item to
  its brace-balanced end instead of to end-of-file (the guards had been
  blind to ~580 production lines of zensight-common/src/config.rs); the five
  hand-kept guard path lists collapse into one computed list with the single
  named exemption (conformance's deliberate un-namespaced sessions) stated;
  the session guard also matches `use …::open` imports; the D2 colour guard
  matches the constant/struct/macro constructors and caught one live
  violation (chart.rs `Color::BLACK` → a named theme accessor). Six shipped
  configs (snmp/gnmi/modbus/netflow/netlink/rerun) that nothing ever parsed
  gain `shipped_config_parses` tests. Every third-party action is pinned to
  a commit SHA (the toolchain action now names its toolchain explicitly —
  with a SHA the ref no longer carries it) and the bpf-linker download is
  checksummed. `--locked` on the three behavioural scripts' builds; every
  job has a timeout; release/features-ebpf gain concurrency groups. cargo-
  deny's bans stay `warn` deliberately (102 duplicate-version skips would
  rot faster than they protect — reasoning recorded in deny.toml), and the
  new scheduled `deny-fresh` workflow re-checks advisories with the ignore
  list stripped, so a stale ignore surfaces instead of sleeping — the same
  staleness class that hid the field-new lift for a release.

### Fixed

- **The systemd watchlist cap no longer drops exact-named units** (#865).
  `watch_max` truncated matches in D-Bus `ListUnits` order — an order that
  means nothing and happens to lead with sockets — so a wildcard-heavy
  watchlist (the demo: 96 matches vs cap 50) dropped every explicitly-named
  service. Those are precisely the units with IP accounting, which killed
  their `ip_*_bps` series (the GUI Bandwidth Services table rendered empty)
  and their threshold-alert inputs, while the operator's nine deliberate
  patterns were silently not honored. Exact (non-wildcard) patterns now
  always survive the cap; wildcard matches fill the remaining room sorted by
  unit name (publication order of watched units is now name-sorted, no
  longer D-Bus arrival order); drops are logged by name — exact drops at
  warn, wildcard drops as a count plus a first-10 sample, the full list at
  debug. The demo config also raises `watch_max` to 100 so the Timers /
  Sockets panels see the whole curated match set truncation-free.

- **The memory governor no longer thrashes when the budget is below the
  process baseline** (#864). A budget under netring's ~297 MiB capture-ring
  baseline armed the #812 shed ladder from second one with an unreachable
  eviction target, so every 5 s tick LRU-wiped all seven L7 inventories
  (tls/dns/http/asset/quic/ssh/enc_dns — a few KB against a ~160 MiB
  shortfall) and five GUI views rendered empty for the life of the process.
  The ladder now latches **futile** when a round frees under 1 % of its
  target: eviction stops (the tables keep their data), the ladder climbs to
  Saturated and holds, and the health doc says why —
  `self_stats.ladder.futile` (new, additive) plus a "raise budget_rss_mb"
  reason. The latch clears on a budget resize or genuine relief below the
  clear line; recovery needs no restart. Thrashing, like dying, is not on
  the ladder. The deployment fact that exposed it is also fixed: the demo
  budget rises 128 → 448 MiB and the production profile's 64 MiB sed is
  deleted — both profiles measure the same ~297 MiB idle baseline
  (2026-08-31, VmHWM), because detectors-off saves table churn, not rings.

- **systemd: a unit with an uppercase name no longer breaks the telemetry
  guard** (#843, found recording the #747 fixture corpus). `sanitize_unit`
  hand-mapped reserved characters to `_` but never folded case, so
  `NetworkManager.service`-style units produced chunks the grammar refuses —
  a debug-build panic in the collector task, unregistered keys in release —
  and its `→ _` substitution was not injective (`user@1000.service` collided
  with `user_1000.service`). It now delegates to `zenkey::Chunk::slug`, the
  sanctioned foreign-value boundary: already-legal names stay byte-identical;
  everything else gets the RFC 03 §2 injective `_xNN_` escape (breaking only
  for keys that were previously broken or colliding). The raw name still
  rides every point's `unit` label.

- **`cargo test -p zensight` no longer segfaults on GPU-less hosts** (#829,
  the #687 landmine): the test binaries now set `WGPU_BACKEND=gl` themselves
  via pre-main `ctor` guards (`src/lib.rs` for `--lib`, `tests/ui_tests.rs`
  for the integration target), so the parallel-test Vulkan/lavapipe crash
  cannot occur regardless of how the tests are invoked. An explicit
  `WGPU_BACKEND` still wins; `just test-ui` remains as the discoverable name.
  Measured: 0 crashes in 20 `--test ui_tests` + 10 `--lib` parallel runs,
  against ~1-in-7 and ~1-in-3 before.
- **The alert plane's QoS agrees with the ratified profile: `express` is on
  for `QosClass::Alert`, and for it alone** (#830). zenkey RFC 04 §3's
  `alert` profile declares express ("rare and must-arrive, since v1.26");
  `QosClass::express()` generalized the media-plane argument (#733) to the
  whole table, so every live alert crossing a conformance window drew a
  correct `qos-observed-mismatch`. Wire-behaviour change on the alert plane
  only; the reasoning moved through `zensight-sensor-parallax/docs/qos-express.md`,
  which now carries the alert carve-out.
- **Alert and event puts are encoding-stamped** (#830). `publish_raw` now
  takes the caller's encoding and stamps it (RFC 08 §7), so consumers resolve
  alert/event payloads from metadata instead of the first-byte sniff — the
  sniff that read an empty tombstone as CBOR and manufactured a
  `payload-undecodable` error in the conformance gate. The judge-side half
  (a `Delete` tombstone must not be decoded as a value) is zenkey-fleet
  0.11.1's doctor fix; `zenkey-fleet` is bumped to 0.11.
- **The alert seed rides the reporter's format** (#830). `serve_alerts_query`
  hardcoded JSON while live samples used the reporter's `Format`; they agreed
  only because every sensor passes `Format::Json` today. A CBOR reporter now
  seeds CBOR, pinned by test.

## [0.11.0] - 2026-08-28

A month of unreleased work, and the release an operator has to read before
upgrading. Seventeen entries below alter a wire contract, an exported series
name or a deployment default; three of them break a *deployment* rather than an
API.
Start with **Changed — BREAKING**, and with the alert re-key in particular:
alerts are LWW state keyed by the thing that changed, so every alert firing at
the moment of upgrade strands a phantom at its old key that nothing will ever
clear. [`RELEASING.md`](RELEASING.md) carries the sweep, and which deployments
need it.

The headline features are the adaptive `@media` plane — a viewer that moves
itself down a rung, a stream health panel that names the stage losing the
picture, and drop accounting sourced from the sink instead of inferred — and
`zensight-conformance`, which now asks a *running* fleet in CI whether it obeys
its own contract.

#### Upgrading to 0.11.0

In this order:

1. **Upgrade the whole fleet together.** zblob's wire went v1 → v2 → v3 in this
   cycle and the versions deliberately do not interoperate, so **sensors and
   frontend must move together** — a mixed fleet fails closed on artifact
   transfer rather than half-decoding it.
2. **Then sweep the stranded alerts** — only where a Zenoh storage is pointed at
   `v1/*/state/**`, and only once no old-version producer remains. `alert_key`
   moved to the normative RFC 11 §3.1 derivation (#736, #738), so every alert
   firing at the moment of upgrade leaves a phantom at its old key that nothing
   will ever write again. The procedure, the who-needs-it table and the reason
   it is a GET-then-delete of concrete keys rather than a wildcard delete are in
   [`RELEASING.md`](RELEASING.md) § *Migration: re-keying the alert state on
   upgrade*. Doing it now is cheap: nothing consumes alerts yet.
3. **Re-point every Prometheus scrape config and firewall rule** that named
   `0.0.0.0:9090` at `127.0.0.1:9464` (#771).
4. **Expect dashboards and recording rules to break, and fix them from the
   entries below.** Every exported series for the SNMP sensor is renamed
   (#764, #765, #779, #783, and the 49-name table under the SNMP rename entry);
   text telemetry becomes an `_info` family (#752); OTel counters become
   asynchronous instruments (#754); and thirteen sysinfo levels change type from
   counter to gauge (#766), which is a payload change every consumer sees.
5. **OTel operators**: each host now has its own `Resource` (#755), so queries
   keyed on the single fleet-wide resource must be re-pointed — and if an OTLP
   endpoint was configured with headers, TLS or HTTP multi-signal, re-read
   #756; that configuration did not previously work.

Configs referencing `snmp.mib.files` no longer load (#580), and anything
building from `docker/Dockerfile.exporter` needs a new path (#778).

### Changed — BREAKING

- **The SNMP CPU, IP-address and storage tables are registered subject trees, so
  `sum by (index)` works for them too** (#783, registry `snmp` 1.8 → **1.9**).
  The finish of what #779 started for `ifTable`/`ifXTable`.

  While a key matched only snmp's rest-var catch-all `{device}/{metric...}`,
  #764's family rule had no literal chunks to name from and fell back to the
  rest variable's **value** — so the table index rode in the metric *name*.
  `zensight_snmp_storage_1_size` and `zensight_snmp_storage_2_size` were two
  unrelated families; `zensight_snmp_cpu_1_load` and `zensight_snmp_cpu_2_load`
  likewise. #769 had already attached the index as a **label**; these patterns
  are the other half, and now the generic rule takes over:

  | | |
  |---|---|
  | before | `zensight_snmp_storage_1_size`, `zensight_snmp_cpu_2_load` — one family per row |
  | after | `zensight_snmp_storage_size{index="1"}`, `zensight_snmp_cpu_load{index="2"}` — one family, aggregatable |

  **Any dashboard or recording rule naming `zensight_snmp_{cpu,ip,storage}_<n>_<column>`
  breaks.** Nothing in-tree does — `demo/prometheus/dashboards/` ships no SNMP
  panel, and `dashboards-blocked/README.md` (updated here) explains why: a panel
  is provisioned only after it has been checked against a real device polled by
  a running exporter, and nobody has done that yet.

  Registered **column by column**, not as `{device}/<table>/{index}/{column}`,
  for the reason #779 paid to learn: a single `{column}` variable is dropped
  from the family name along with every other variable, collapsing a table's
  columns into one family carrying counters, gauges and strings at once — two
  `# TYPE` lines for one name, the scrape-killer class #752 fixed.
  `storage_columns_are_not_collapsed_into_one_family` pins that half.

  **Two deliberate differences from #779**, both worth stating because their
  absence looks like an oversight:

  - **No `.rate` siblings.** The poller derives a rate from the *wire tag* —
    `Counter32`/`Counter64`, or a `Gauge32` the MIB declares as a counter — and
    not one column of these three tables is a counter: hrStorage is INTEGER
    throughout, hrProcessorLoad is INTEGER, ipAddrTable is IpAddress/INTEGER.
    ifTable needed them because `in_octets` and friends genuinely are counters.
    Registering twelve dead `.rate` patterns to look symmetrical would make the
    registry advertise a surface no build can emit, which is the class of lie
    RFC 08 §6.1 and #484 exist to prevent.
  - **The `ip/` group's scalars stay on the catch-all** (`ip/forwarding`,
    `ip/default_ttl`, `ip/in_receives` and its `.rate`). They are 3-chunk keys,
    so the new 4-chunk patterns cannot match them, and their rest-var family
    name already carries no index — there was nothing for a registration to fix.
    `the_ip_scalars_are_untouched_by_the_indexed_patterns` pins that they do not
    get swept into an indexed family, and that they carry no `index` label.

  Not a wire change: the poller publishes exactly the same keys with exactly the
  same labels. Only the exporters' *reading* of them moves. `registry.lock`
  regenerated (10 additive entries), and
  `the_catch_all_would_still_bury_the_table_index_in_the_name` pins the
  before-state so the improvement is demonstrated rather than asserted.

- **Every firing alert re-keys: `alert_key` is now the normative RFC 11 §3.1
  derivation** (#736, #738). ZenSight had its own recipe with the same hash
  (FNV-1a-64) and the same 16-lowercase-hex output, but two byte differences
  from the spec: the framing put `\0` *after* the rule and after each `k=v`
  rather than `\n` *before* each label with none trailing, and the exclusion
  matched only the `host.` prefix, so the RFC's own bare `host` label was
  hashed in. `Alert::alert_key()` is now a thin wrapper over
  `zenkey::alert::alert_key`, so two independent implementations mint the same
  key for the same alert. The RFC's test vector — rule `link_down`, labels
  `{peer: r2, port: eth0, host: h-3fa9c2d41b7e}` → `a659f813308ad1da` — is
  pinned in `alert.rs`; that same alert used to key as `c25da085d5c5b7e7`.

  **Alerts are LWW state keyed by the thing that changed**, so every alert
  firing at the moment of the upgrade leaves a permanent phantom at its old key
  that nothing will ever clear. See [`RELEASING.md`](RELEASING.md), "Re-keying
  the alert state on upgrade" (#737), for the sweep — it is a
  GET-then-delete-per-concrete-key enumeration, run **after** every publisher
  is upgraded, and it is needed only where a Zenoh storage is pointed at
  `v1/*/state/**`. `just run` and the e2e suites carry no persistent state and
  need nothing.

  **`host.*` stays excluded, and that is byte-normative, not a deviation.**
  RFC 11 §3.1 excludes "the label named `host`, *and any label the producer
  documents as host-scoped*", because only the producer knows its vocabulary.
  ZenSight's is the `host.` annotation namespace, now declared in code as
  `zensight_common::alert::{HOST_SCOPED_PREFIX, is_host_scoped}` and in
  `docs/KEYSPACE.md`. Excluding it is load-bearing: `AlertReporter.active` is
  keyed by `alert_key()` and the resolve path re-derives it, so an identity
  refresh between fire and resolve would leave the `Firing` on the old key
  forever while the `Resolved` and its tombstone landed on a new one — a
  permanent phantom, with nothing logged.
  `a_host_annotation_change_does_not_orphan_a_firing_alert` in
  `zensight-sensor-core/tests/alert_reporter.rs` is that invariant; it fails if
  the host-scoped vocabulary is ever dropped from the wrapper.

  `Alert::alert_key()` stays **infallible**: it is called from ~35 places, and
  the errors `zenkey::alert::alert_key` returns are framing-injectivity
  violations (a `\n` in a rule forges a label) that no ZenSight rule produces.
  On refusal the offending bytes become `_`, a WARN names the rule, and the
  normative derivation runs on that — deterministic, so a `Firing` and its
  `Resolved` still agree.

- **`zenkey` and `zenkey-build` 0.6 → 0.7** (#735). The wire is unchanged —
  the `identity.rs` golden host-id vector (`h-` + first 12 hex of
  `sha256(machine_id + salt)`) still passes, so no origin re-keys — but three
  API surfaces moved, and one of them was silently wrong before.
  - `V1Context::for_producer` is now `Result<Self, KeyError>`. 0.6 slugged an
    illegal producer name and, failing that, fell back to the literal
    `sensor`: a misconfigured producer published its **entire keyspace under a
    different identity**, with no `Err`, no panic and no log, colliding with
    every other misconfigured producer in the fleet. ZenSight absorbs the new
    `Result` **once**, in `zensight_common::v1::for_producer` (re-exported as
    `zensight_sensor_core::v1::for_producer`), rather than threading `?`
    through 47 call sites: a ZenSight producer name is a compile-time constant
    from `zensight-common/registry/`, and a new test asserts every registered
    name is chunk-legal, so an illegal one now fails `cargo test`. That change
    found four real cases — the logs and systemd test harnesses were passing
    `test_<nanos>/logs` as a *producer chunk*, which 0.6 had been quietly
    renaming into something else.
  - `V1Context::state_key` / `rpc_key` are now `Result` too, for one reason: a
    chunk that is literally `alive`, the reserved liveliness leaf (RFC 03 §3).
    `zensight_common::v1::V1ContextExt::{const_state_key, const_rpc_key}`
    carries the constant-subject case; the two builders whose chunks really
    are foreign data — an SNMP device name, a parallax stream name — now
    *refuse* a device or stream called `alive` and log it, instead of minting
    a key that collides with that producer's liveliness token.
  - `AppProfile::new` takes `AppName` / `OriginSalt` newtypes, because
    `AppProfile::new("zensight-host-id-v1", "zensight")` used to compile and
    re-key the whole fleet. Both constructors stayed `const fn`, so
    `zensight_common::PROFILE` is still a plain `static`.
  - `StructuralKey::producer` became a method (`Position5` now holds
    producer-or-blob-tier-or-chunk), `ServiceOrigin` is a newtype rather than
    a `String`, and `SubjectDecl::class` is a typed `Declared<Class>` — the
    last of which broke `registry_audit.rs` at **compile time** rather than
    silently returning an empty `Vec`, which was the risk.

- **The Prometheus exporter's scrape port default moves `0.0.0.0:9090` →
  `127.0.0.1:9464`** (#771). 9090 is the Prometheus *server's* own port, and the
  shipped `README.md` told you to scrape `localhost:9090` — i.e. Prometheus
  scraping itself. Any stack running both on one host collided. It also now binds
  loopback rather than every interface; the container and systemd deployments set
  `listen` explicitly and are unaffected. 9464 is the conventional
  OpenTelemetry/Prometheus-exporter port.
- **Text telemetry is exposed under an `_info` family with a named label**
  (#752). A `TelemetryValue::Text` point used to render `# TYPE <name> info` —
  and `info` is an **OpenMetrics** type that the `version=0.0.4` text format we
  serve does not admit. Prometheus's parser aborted on the unknown token and
  **rolled back every sample in the scrape**, while the target still reported
  healthy. One netlink MAC address, one SNMP `sysDescr` or any gnmi string was
  enough to empty the whole endpoint. Text now renders as
  `<name>_info{…,<leaf>="<text>"} 1` with type `gauge`, the text rides under a
  label named for the subject leaf instead of a literal `value`, and the value is
  stripped of control characters and clamped to 128 bytes. `/metrics` and
  remote-write spell the series identically. Opt out with
  `prometheus.export_text_metrics: false`.
- **OTel counters are exported through asynchronous instruments** (#754).
  `counter.add()` was being fed the **absolute** device reading, so under
  cumulative temporality the exported Sum became a running total of absolute
  readings — an interface at 1 000 000 octets reported 1e6, 2e6, 3e6, forever.
  Every `rate()` was meaningless and the series never decreased across a counter
  reset. `TelemetryValue::Counter` is already the cumulative total, so it is now
  reported by an `ObservableCounter` rather than added to. This also gives stale
  series a real **gap** instead of a flat line: `cleanup_stale_observations`
  (previously dead code with zero callers) is now wired to a sweep task, and an
  evicted series stops being observed.
- **Filesystem, memory and TCP levels are published as gauges, not counters**
  (#766). Thirteen `publish` sites typed a *level* as `TelemetryValue::Counter`
  — memory total/used/available and their swap siblings, the per-mount
  filesystem levels, per-process memory, the TCP per-state connection counts,
  uptime and boot_time. All of them go down. On Prometheus that emitted
  `# TYPE … counter` for a decreasing value, so every `rate()`/`increase()` over
  them was nonsense and the counter-reset heuristic fired on every deletion; on
  OTel, once #754 made counters honest, a monotonic Sum that decreases is a
  contract violation rather than a bad panel.

  **This is a wire payload change**: the serde tag flips from `"counter"` to
  `"gauge"` and every consumer sees it (GUI, correlator, rerun, both exporters).
  Genuine counters are untouched — network rx/tx bytes/packets/errors and the
  disk I/O byte/op/time totals stay `Counter` and still render `_bytes_total`.
  `exposition::KIND_OVERRIDE` corrects the same keys arriving from a sensor that
  has not been upgraded yet, because an exporter has to be right against
  whatever is actually on the bus. The tell was a doubled suffix once #767
  landed the conventions — `zensight_system_memory_usage_bytes_total` — which is
  how the full set was found rather than guessed at.

- **Every host gets its own OTel `Resource`** (#755). `build_resource_attributes`
  emitted `service.name`, an optional `service.version` and whatever the
  operator hand-wrote, so the entire fleet shared **one** Resource despite every
  origin being literally `h-<12hex>`. For metrics that reads as
  `job="zensight", instance=""`. For logs it is worse: a backend derives stream
  identity from the resource, so the fleet collapsed into a single Loki stream
  with the host demoted to structured metadata, and spans carried no host at
  all.

  OTel binds a Resource to a *provider*, not to a record, so one resource per
  host necessarily means one provider set per host. `SignalStack` is that set,
  built lazily on the first sample from an origin and capped by `max_resources`
  (512); past the cap a host falls back to the shared resource and is counted —
  degraded but honest rather than unbounded. The resource carries `host.id` (the
  RFC 06 minted origin, stable across hostname changes, which is why it and not
  `host.name` is the identity), `host.name`, a `service.instance.id` that
  includes the producer instance so `netring-2` is distinguishable from
  `netring`, and a per-producer `service.name` so a service map means something
  rather than showing one node called "zensight". Operator `resource` attributes
  merge *underneath* and cannot override observed truth. **A dashboard or log
  query keyed on the old single resource must be re-pointed.**

- **The OTLP connection was unusable as configured** (#756). Three defects that
  between them meant the exporter worked only against a local, plaintext,
  single-signal collector — while the README advertised Grafana Cloud,
  Honeycomb, Datadog, New Relic and Azure Monitor.

  - `protocol: "http"` **could not export more than one signal**.
    opentelemetry-otlp takes a *programmatic* endpoint verbatim and appends
    `/v1/<signal>` only when falling back to the environment, so all three
    signals POSTed to `/` and the collector 404'd everything — and because one
    config field served three signals, appending `/v1/metrics` by hand fixed
    metrics and broke logs and traces. `endpoint` is now a base with optional
    per-signal overrides, and the path is appended under HTTP only (gRPC routes
    by service name, not path). A base that already carries the path is left
    alone, so an existing config keeps working.
  - `headers` **was dead config** — defined, parse-tested, and applied nowhere,
    so an authenticated backend answered 401 with no hint why. Wired for both
    transports now, names lower-cased, an unrepresentable name or value failing
    at startup rather than silently.
  - **gRPC + TLS killed the process at startup**, and the usual diagnosis was
    wrong: TLS is not missing upstream. This crate simply never enabled a TLS
    feature, so an `https://` endpoint hard-errored out of `OtelExporter::new`.

- **`docker/Dockerfile.exporter` is deleted** (#778). Referenced by nothing —
  not compose, not CI, not the justfile — with a dead `EXPORTER_NAME` arg, a
  `CMD ["--help"]` and no config baked in, and an `EXPOSE 9090` encoding the port
  collision above. The images that ship are built from `Dockerfile.runtime`.

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



- **Metric names come from the registry, not the payload** (#764). Neither
  exporter parsed the key: both named from `point.protocol` + `point.metric`, so
  a per-entity subject landed in the metric **name** — `disk/root/inodes_total`
  and `disk/srv_dev/inodes_total` were two unrelated metric families, and
  `sum by (mount)` could not be written at all. Only sysinfo and systemd
  escaped, through the hand-written semconv table.

  The primitive already existed and everyone else already used it:
  `keyexpr::refine_key` does registry-backed refinement and is called by the GUI
  and the correlator, and #475 established "decode keys through the registry,
  not `split('/')`". The exporters were the last consumers that never migrated.
  `exposition::identify` now derives the **name** (a semconv hit wins;
  otherwise the registered pattern with its `{var}` chunks dropped, prefixed by
  the producer — a rest-var producer like snmp/modbus/gnmi/netflow is named from
  the rest variable's value, which preserves exactly what those producers
  already exported while promoting `{device}` from buried-in-the-name to a
  label), the **labels** (`origin`, `source`, `protocol` and the producer
  instance as structural truth, then semconv constants, then pattern vars) and
  the **unit** (registry `unit()` → `point.unit` → a consumed `unit` label).
  This is the mechanism behind the SNMP renames below.

- **Semantic-convention entries are keyed on (producer, registry pattern)**
  (#765). Each arm used to re-split the metric string to compute its attribute
  *values*, so `disk/sda/io/read_bytes` produced `device="sda"` here while the
  sysinfo sensor produced `device="sda"` as a point label — two producers of one
  value, and the exporters emitted both, which is an invalid series (#753). The
  systemd half already carried a workaround for exactly this, leaving its
  attributes empty with a comment that the point "already carries the `unit`
  label"; that patched one symptom by omission and left the sysinfo one live.

  An entry now only ever **names** an attribute — `constants` for what the
  pattern cannot supply (state, direction, type) and `var_renames` for
  registry-variable → semconv-attribute — with values coming from
  `AnySubject::vars()`, the registry's own parse. One producer per value, so the
  duplicate is unrepresentable at the source rather than caught downstream. The
  renames are the substance: filesystem's semconv `device` comes from
  `{mount}`, network's from `{iface}`, and per-core CPU's `cpu` from `{core}` —
  a table assuming identity would have had to compute those itself, which is the
  original bug. systemd units now name their `unit` attribute honestly instead
  of omitting it.

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
  `docker/configs/snmp.json5` was shipping exactly that, and is fixed here —
  though see the Removed entry below: that file turned out to ship nowhere.

- **The deprecated JSON pseudo-MIB support is removed** (#580). `snmp.mib.files`
  is now a hard startup error pointing at `snmp.mib.dirs`, rather than a
  warning. Deprecated in #532 and warned through 0.10.x.

- **The `@media` origin is a type, and there is no wildcard** (#649).
  `media_video_key`/`media_preview_key` take a parsed `zenkey::RemoteOrigin`,
  drop their `protocol` parameter, and have no `format!` fallback. Both stated
  reasons for that fallback were already dead: the `*` origin covered "a viewer
  subscribing before the origin map fills", a window that has not existed since
  #474, and `parallax.toml` is the registry's only `[[media]]` declarer, so
  `protocol` had exactly one legal production value.

  Deleting the `*` arm is the point rather than tidying. RFC 07 §3: a consumer
  that cannot name an origin still MUST NOT fan out across origins on `@media`
  or `@blob`, because every matching holder ships the full payload and Zenoh
  cannot cancel remote replies in flight — one camera named `cam0` per host
  meant N× the video arriving at one viewer. A typed origin makes the wildcard
  unrepresentable rather than discouraged.

### Added

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

- **`StreamStatus` says why a stream stopped, and the GUI stops inventing
  sentences** (#691). A stream the operator closed and a stream whose camera
  was unplugged both published `StreamStatus { open: false, tiers: [] }` — one
  bit — and the viewer filled the silence with `"stream ended"` or `"stream
  failed to open on the sensor"`, guesses that were wrong as often as right.

  The information existed the whole way down and was destroyed twice. parallax
  0.7 gave the egress loop a typed `EndReason::{Eos, Error(StreamError{node,
  message}), Aborted}` (#689); `egress::run` flattened it to
  `Result<(), String>`, merging `Eos` with `Aborted` and stringifying the
  element's name into its own message. `handle_egress_ended` then consumed the
  string into device health and — the structural part — called
  `teardown_profile`, which removes the slot, **before** `publish_status`,
  which reads nothing but the slot map. The actor deleted the evidence one line
  before it published.

  `StreamStatus` now carries `last_end: Option<StreamEnd>` — `{ tier, reason }`
  with the tier **named**, and eight reasons in three families: we ended it
  (`closed`, `idle`, `superseded`, `shutdown`), it ended itself
  (`source_ended`), or it failed (`stalled`, `failed { node, message }`,
  `failed_open { message }`). `is_failure()` is the one predicate health and the
  RTSP alert gate on, so the families cannot drift apart.

  Three of those distinctions are new information, not relabelling. **`closed`
  vs `idle`**: a `close_stream` does not stop a pipeline — it releases a
  refcount and the idle countdown does the stopping — so a clean close and the
  crash backstop for a viewer that died without saying goodbye arrive through
  one reaper. The refcount at reap time tells them apart, and an operator could
  previously see neither. **`stalled` vs `failed`**: the first-frame watchdog is
  precisely the case where nothing failed and there is no error to quote, so
  none is invented and it carries no payload. **`failed_open` vs `failed`**: one
  says check the config and whether the camera is reachable, the other says
  check the element `node` names, and a late-joining consumer cannot recover
  that difference from context.

  `StreamEndReason: Display` is the single source of this prose, so the
  producer's log line, device health's `last_error` and the viewer's tile
  caption are now literally the same sentence. On the GUI side `TileState.ended`
  becomes a `TileEnd` that records *whose account it is*: the producer's beats
  the viewer's in either arrival order, and a failure is coloured `danger_text`
  rather than sharing the muted grey a clean close gets.

  Additive and `skip_serializing_if`'d, so old and new peers decode each other
  either way — and **absent means no tier has stopped since this stream last
  opened**, never "stopped for an unknown reason". No registry version bump and
  no `zenctl registry lock` run: the compat lock pins path, class and type
  *name*, not payload shape (`zenctl registry lock` reports `added: 0`). The
  subject's `description` is refreshed regardless, since the contract moved.

  Health was checked rather than changed: the producer's own teardowns already
  could not count as device failures, because `teardown_profile` removes the
  slot and aborts the egress task before an end can be reported, and a late one
  dies on the epoch guard. That invariant is now pinned by tests instead of
  being a property nobody had written down.

- **A losing Transport hop now says whether the sender is congested or the link
  is dropping** (#801, epic #712).

  #719 named the hop. Naming Transport turned out to be half an answer: two
  opposite faults wear that name, and #713 measured both producing *identical*
  counters — `stats/drops` reads **zero** whether a `tcp/` link is congested
  (83 % of sequences missing at 300 kbit) or a QUIC link is dropping packets
  (20 % missing at 1 % loss), because congestion discards frames inside Zenoh's
  own transport queue, upstream of every counter the sensor has.

  What separates them is **frame age**, by three orders of magnitude: 3 502 ms
  and 9 085 ms under congestion against 0.77 ms and 0.78 ms under in-flight
  loss. Above 500 ms a losing Transport hop now reads "the sender is congested.
  What does arrive is 3.5 s old, so the frames were discarded before the wire
  and no counter here saw it"; below it, "they were lost in flight … nothing is
  queueing; the link is dropping."

  Deliberately **not** the new counter the issue was opened for: Zenoh counts
  transport drops only under its `stats` cargo feature and only per *link*
  (`zenoh_stats::LinkStats`), never per publisher, so a link-level number under
  a stream's key would be an unattributable number wearing an attributable
  name. Frame age is already measured, already reported, and already
  per-stream.

  The test runs only once the hop is already losing ≥ 15 %, so a fresh stream
  with a leisurely age is not accused of anything, and an unstamped stream keeps
  the location without a cause — "not asked" is not "answered no".

- **The viewer moves itself down a rung — receiver-driven tier selection** (#720,
  epic #712).

  #502 gave a viewer a per-tier button; this is the same decision made every
  three seconds from the tile's own [`MediaReceiverReport`]. The report was
  already being computed and sent (#718); the controller is a second read of it.

  **The viewer changes its own subscription. It never asks the sensor to
  re-tune an encoder.** RFC 07 §1.2 is normative — two operators on different
  links watch the same camera, and one asking for less must not degrade the
  other — so the only lever is which `<tier>` key the tile subscribes to, and
  the feature adds no wire surface at all.

  Downgrading is not merely cheaper here, it is *repair*: #713 measured loss
  being amplified by access-unit size (1.5 % of an 842 B unit, 20 % of a 34 KB
  one, 41 % of a 136 KB one, all at 1 % packet loss), so halving the bytes per
  frame roughly halves the chance a frame is lost at all. Frame age is a
  first-class input for the same reason — on `tcp/` the measured failure mode
  was 3.5–9 s of age with the sensor's `stats/drops` at zero.

  Three inputs (loss, frame age, decode-queue occupancy), each with **two**
  thresholds and never one comparison flipped: any one triggers a downgrade,
  all three must be healthy for an upgrade. Plus a 12 s minimum dwell, a 9 s
  post-switch cooldown whose reports are **discarded rather than averaged**
  (they describe the decoder rebuild, and folding them in teaches the
  controller that switching causes the problem switching just fixed), and 30 s
  of continuous health before any upgrade. A move at the end of the ladder is
  not a move: no switch is sent and the dwell is not reset, because resetting
  it is how a controller already on the bottom rung starves itself of the
  recovery window it is waiting for.

  Absent inputs stay absent — unstamped samples drop the age test rather than
  reading as zero, and an unset deadline means the operator asked for no
  latency policy — and rung order comes from `TierSpec::bitrate_kbps` rather
  than from the order the catalogue lists tiers in.

  **The human always wins.** An explicit tier click pins the stream; an `Auto`
  button appears beside the tier buttons while pinned and hands control back.
  There is no separate off switch: a pin *is* off, for the one stream the
  operator pinned. Closing the tile drops the pin with it.

- **What `@media` loss actually looks like — measured, and the recovery rule
  written down** (#713, #721, epic #712).

  Every knob in the adaptive-media epic — the frame-age deadline, the report's
  loss field, the controller's downgrade point — was a number aimed at a loss
  distribution nobody had produced. `scripts/media-loss-lab.sh` produces it: two
  network namespaces joined by one veth, netem or tbf on the sender's egress
  only, everything torn down by the EXIT trap and no qdisc ever attached to `lo`
  or a real interface. `zensight-sensor-parallax/examples/media_loss_probe.rs`
  records one row per sample and never decodes, sheds or asks for a keyframe, so
  the CSV is the wire rather than the wire plus a policy;
  `scripts/media-loss-report.py` turns the rows into the tables.

  **Two findings, both in
  [`docs/plans/adaptive-media/loss-measurement.md`](docs/plans/adaptive-media/loss-measurement.md).**

  Over `tcp/` — every configuration we ship — congestion loses frames and *no
  counter says so*. At 300 kbit against ~1.7 Mbps offered the receiver missed
  **83 % of sequence numbers while `stats/drops` stayed at 0**, with frame age at
  **3.5 s median**; at 100 kbit, 93 % missing and 9.1 s. The frames died in
  Zenoh's transport queue under `CongestionControl::Drop`, upstream of
  `stats/drops` (at the time inferred at egress from AppSink sequence gaps;
  #692 has since sourced it from the sink's own counter, which does not change
  this finding — these frames die *downstream* of the sink either way). The epic's
  standing caveat said best-effort was "a no-op in flight" and implied the
  resulting drops would at least be *visible*; the first half holds and the
  second does not. Filed as #801.

  Over `quic/…?mixed_rel=1` best-effort really does ride unreliable datagrams,
  and loss is amplified by **access-unit size**: at 1 % packet loss, a 842 B unit
  was lost 1.5 % of the time, a 34 KB unit **20 %**, a 136 KB unit **41 %**. The
  strict `1-(1-p)^n` fragment product over-predicts (increasingly with size; UDP
  GSO is the likely reason and is named rather than assumed), so it is an upper
  bound. Loss is also **bursty in frames** — up to 10 consecutive at 5 %, up to 27
  under TCP congestion — which is why #720's input must be gap burst length and
  not a mean rate.

  Two verdicts fall out. `max_slice_len` (#509) changes nothing on this plane,
  because the sensor publishes a whole access unit as one sample — measured, not
  merely restated. And `express` (zenkey #304) has nothing left to decide: the
  damage on the congested leg is queueing, which per-message framing does not
  touch.

  #721 is the rule those numbers justify, in
  [`docs/plans/adaptive-media/recovery-policy.md`](docs/plans/adaptive-media/recovery-policy.md)
  with the durable half in `zensight-sensor-parallax/docs/streams.md`: repair is
  worth it only while it beats the frame's deadline, which on the RF and
  satellite links `zenoh-modem` targets it never does. So v1 is drop-stale plus
  one keyframe request, paced by wall-clock — and the two conditions that would
  reopen FEC or retransmission are written down so the next proposal can be
  answered with a link.

- **The stream health panel: which stage is losing the picture** (#719, epic
  #712).

  #503 put real resolution, bitrate and fps on a tile caption; this is the
  drill-down that says what they mean. Expand a tile and the panel sits between
  the caption and the picture.

  It is a **chain**, not five gauges, because the verdict is always a
  comparison between adjacent stages and five gauges make the reader do the
  subtraction: offered 30 with encoded 12 is an encoder verdict, encoded 30
  with received 12 is a transport verdict, received 30 with decoded 12 is a
  decoder verdict. The worst hop is named in one sentence above the chain —
  that sentence is the feature, and the numbers are in service of it. A hop
  must lose ≥ 15 % before it is named: rates jitter by a few percent between
  three-second windows, and a panel that shouts at 3 % teaches an operator to
  ignore it.

  Three honesty rules it is built around. **The first link is an offer, not a
  measurement** — capture fps is on no key, so the chain starts at the tier's
  applied fps and says `offered` rather than `measured`, because presenting a
  config value as a measurement is how a panel lies. **A rate needs two
  reports** — the wire counters are cumulative, so the tile keeps the previous
  one and the panel diffs it over the newer one's `interval_ms`; a counter that
  went backwards (a reopened tile) yields no rate rather than a negative one.
  And **missing inputs read as `not asked`**, the same vocabulary the fleet
  view uses: an unstamped stream shows frame age unavailable, never `0 ms`, and
  a preview tile shows `no queue`, never `0`.

- **The media tiles' receiver half: a frame-age deadline, a bounded decode
  queue, and a tile that reports** (#716, #717, #718 — epic #712).

  #714/#715 built the surface a consumer feeds back through and nothing spoke
  into it. This is the consumer. Full contract:
  [`zensight/docs/media-receiver.md`](zensight/docs/media-receiver.md).

  **Every frame that does not reach the screen now has exactly one cause.**
  `decode_to_rgba` used to answer `Ok(None)` for *both* "the decoder is
  buffering" and "the arena had no free slot", so a tile losing a third of its
  frames to a starved arena looked exactly like one losing them to the network,
  and nothing counted either. It now returns `Decoded::{Picture, Buffered,
  ArenaFull}` and `DecodeFailure::{Oversize, Codec}`, and every shed is counted
  by cause: a deadline miss, a full queue, an undecodable delta, a preview
  superseded in the latest-wins drain, a starved arena, an oversize access
  unit, a decode refusal. Sequence gaps stay in `lost_frames` and everything
  above stays in `dropped_frames` — merging them would tell a producer its link
  is bad when the truth is that the viewer's box is too slow.

  **A tile that falls behind sheds instead of drifting.** The H.264 path used
  to decode serially with no backlog drain, so latency grew in the subscriber
  queue where nothing could see it. Access units now go on a bounded 8-deep
  channel that a long-lived blocking task drains, which makes the backlog a
  number readable at any instant — `max_capacity() - capacity()`, the same
  quantity the browser tile reads from `VideoDecoder.decodeQueueSize`, reported
  in the same field with the same meaning — and gives the frame-age deadline
  somewhere honest to be applied. A decoder reset rides that same channel,
  because a reset is a point in the stream and not a side channel.

  **The frame-age clock, stated once.** `zensight_common::media::observed_frame_age_ms`
  implements RFC 07 §1.3 for every consumer we ship: the clock is the
  publisher's HLC sample timestamp, an unstamped sample is *not asked* and
  **never zero** (a `Some(0.0)` there silently disables every deadline built on
  it), and negatives are shown unclamped because a negative age *is* the
  clock-skew evidence. A late **keyframe** is always decoded — shedding those
  too would leave a tile on a genuinely slow link showing nothing at all, where
  taking them gives a slideshow that snaps back the moment the link does.

  **Configurable per deployment, not a constant.** `max_live_latency_ms` in
  `settings.json5` and the Settings view; 1500 ms by default, `0` for off. A
  LAN wall display and a satellite operator want different numbers, and the
  wrong one is either a tile seconds behind live or a tile that sheds
  everything.

  **Both tile kinds report, every 3 s.** Inside the registry's declared ceiling
  of one per second per `(consumer_id, stream, tier)`, and on the tile's own
  clock rather than off arriving frames — a tile receiving nothing still
  reports, and a report saying "nothing is arriving" is the most useful one
  there is. The write is addressed to the tile's own origin with no fleet
  fallback (the registry entry omits `fanout` so a broadcast report is
  unrepresentable); the `consumer_id` is `zs-<pid>-<generation>`, in the
  payload and never in a key; and a report from a replaced tile incarnation is
  dropped rather than forwarded, because forwarding it would keep a dead
  consumer alive in the sensor's per-tier map — which is what
  `rx/{tier}/consumers` counts.

  **Verified against a live sensor**, not only in unit tests:
  `zensight/tests/media_receiver_live.rs` (`#[ignore]`d, `h264`-gated, run by
  hand against `configs/parallax.json5`'s synthetic `test0` stream) drives the
  real tile stream and asserts the loop closes — the report a tile produces is
  accepted by the producer and appears in `{stream}/rx/{tier}/consumers` — and
  that a tile which cannot meet its deadline **still plays**: with a
  zero-millisecond deadline, 8 frames received, 7 deltas shed, 1 keyframe
  decoded, `lost_frames` 0, and two keyframe requests rather than seven.

  Also fixed on the way: the access-unit arena slot goes 1 MiB → 2 MiB, because
  a native-resolution IDR on a high tier could exceed the old one and an
  oversize AU was a hard error the tile resynced at forever. It is now named,
  counted, and after three strikes ends the tile with a stated reason.

- **Receiver feedback on `@media`: `MediaReceiverReport` and the
  `stream/report` procedure** (#714, #715 — the keystone of epic #712).

  A consumer can now tell a producer how the stream is actually *arriving*.
  Until now it could not, and the reason was structural rather than an
  oversight: RFC 04 R6 makes the data planes producer→consumer only, so
  feedback had no home in the grammar at all. zenkey **RFC v1.26**
  (2026-08-25) gave it one, and this is the implementation of §1.1–§1.3.

  **The type.** `zensight_common::stream::MediaReceiverReport` — a *snapshot*
  with counters cumulative since the consumer subscribed, which is what makes
  the procedure's `idempotent = true` true (a delta payload would not be).
  Four loss counters rather than one, so a controller can tell network loss
  from consumer shedding from decoder overload. Five differences from the
  original sketch in #714, each earning its place:

  - **`codec` added, `tier` becomes `Option`** — `(stream, tier)` alone cannot
    name the JPEG preview key, and reusing the `StreamControl` selector shape
    means a report can never name a key an `OpenStream` could not. The sensor
    resolves both through one function, so the two cannot disagree.
  - **`report_ms` → `interval_ms`** — a consumer's wallclock is a *second*
    skewed cross-host clock, and its only plausible use (`now - report_ms`) is
    exactly the laundered-latency mistake RFC 07 §1.3 forbids. A duration is
    consumer-local, skew-free, and is what turns cumulative counters into rates.
  - **`decoder_queue_depth` becomes `Option`** — the iced H.264 tile decodes
    serially and has no queue; `0` would read "queue empty" where the truth is
    "no queue". Same precedent as `stats/rc_drops`.
  - **`frame_age_max_ms` added** — the aggregate must publish both a worst case
    and a typical case, and one scalar per consumer can feed only one of them
    honestly. Median-of-medians and max-of-maxes each mean something.
  - **`last_keyframe_age_ms` → `since_last_keyframe_ms`** — it is consumer-local
    monotonic elapsed, never negative, and must not share a mental bucket with
    `frame_age_ms`, which can be.

  **Absent is not zero, and it is pinned at the byte level.**
  `tests/receiver_report_corpus.rs` carries three CBOR vectors; the one that
  matters is `unstamped.cbor`, a **10-entry map** where a naive encoder would
  emit 16 with nulls. RFC 07 §1.3 makes that normative — where a deployment
  does not timestamp, frame age is *not asked*, **never zero** — and a
  `frame_age_ms` of `0.0` tells a controller the stream is perfectly fresh at
  the exact moment nobody knows. Unlike `framemeta_corpus.rs`, this corpus binds
  no existing twin: it is a *forward* pin for #718's Rust publisher and #722's
  hand-rolled TypeScript one, and hand-rolled encoders get `Option` omission
  wrong first. A negative frame age survives both encodings unclamped, because
  a negative age *is* the skew evidence.

  **The sensor half** keeps the latest report per
  `(consumer_id, stream, profile)`, bounded and aged out on **the tier reaper's
  own window** — `idle_timeout_secs`, one field with two readers and a test
  asserting they agree, because a browser tab that closes never says goodbye and
  the tier reaper already assumes that. The selector refusal is what bounds the
  key space (a consumer cannot mint tier names); the `consumer_id` length check
  bounds the other dimension; over-rate is refused with `error/busy` naming the
  limit, so a caller can back off machine-readably. A well-formed but
  self-contradictory report (`decoded > received`) is **accepted** — that is a
  consumer lying about itself, which the aggregate should show rather than the
  producer hide.

  **The aggregate** publishes per *tier*, not per stream, on
  `telemetry/parallax/{stream}/rx/{tier}/{consumers,loss_pct_*,frame_age_ms_*,decode_queue_*}`.
  A stream-level average would hide the case the whole epic exists for — one
  tier healthy, another not. `consumers` is a real integer, unlike
  `has_viewers()` (a boolean; zenoh's `MatchingStatus` carries no count) — and
  it is honestly a **lower bound** on viewers, since one that never reports is
  invisible to it. That is in the registry description, so nobody later "fixes"
  the discrepancy against `stats/viewers`.

  **RFC 07 §1.2 is normative and this code obeys it structurally.** A producer
  MUST NOT re-tune a shared tier from one consumer's report: two viewers share a
  tier, one reports loss, the bitrate drops, and the *healthy* viewer's picture
  degrades for a reason it cannot see, caused by a peer it does not know exists.
  `command::run` takes a `SessionHandle` because that channel is how a
  `StreamControl` reaches the encoder; **`reports::run` takes an
  `Arc<ReceiverReports>` and nothing else** — no handle, no `SessionMsg`, no
  `PipelineControls`. A comment saying "do not re-tune from a report" is obeyed
  until the next person wires up something helpful; a module that cannot reach
  the knobs is obeyed by the compiler. Three checks keep it that way: a source
  grep in `tests/rfc07_receiver_driven.rs`, the same grep as a CI step so a
  branch that never runs the parallax suite still fails, and
  `reports_never_retune_a_shared_tier` in `tests/e2e.rs`, which points a viewer
  screaming about 95 % loss at a live tier and asserts `TierApplied` never moves.

  Registry `parallax` 1.7 → **1.8**: one procedure, seven subjects,
  `registry.lock` regenerated. The procedure carries a `rate` ceiling because
  RFC 07 §1.1 says a report's rate *"belongs in the registry entry rather than
  in prose"* — but RFC 08 §2 scopes `rate` to `events` subjects and
  `zenkey-build` 0.7 does not lint it on a procedure, so nothing upstream checks
  it. It still reaches the fleet (`introspect` serves the TOML verbatim), and
  `the_registry_declares_the_rate_ceiling_the_sensor_enforces` pins it against
  `reports::REPORT_MIN_INTERVAL` locally. **Worth filing upstream**: RFC 07 §1.1
  assumes a field RFC 08 §2's table does not grant.

  Deliberately **not** in this change: the GUI publisher (#718), the frame-age
  deadline (#716), the decode-queue accounting (#717) — which is why
  `decoder_queue_depth` is `Option` — and the tier controller (#720).

- **`RpcError::busy`** — `ERR_BUSY` has been in the RFC 05 vocabulary since the
  start and had no constructor until a rate-limited procedure needed one (#715).

- **Encode-latency percentiles in stream telemetry** (#729):
  `{stream}/stats/encode_p95_ms` and `{stream}/stats/encode_p99_ms`, read off
  the lock-free histogram parallax 0.8 keeps inside the H.264 encoder. The
  `encoder_overrun` alert is now judged on **p95** rather than the interval
  mean — a stream whose average frame fits the budget while its p95 does not is
  exactly the one that stutters, and overrun is what the rule is named for. The
  mean stays the fallback for the JPEG preview paths, which `TimedElement` times
  but parallax does not histogram.

  `encode_ms` is **not** removed, and neither is `TimedElement`: the histogram
  is all-time (a tail needs history) so it yields no interval mean, it covers
  only the inner `encode()` rather than the whole `process()` call, and it does
  not exist at all for the previews. Registry `parallax.toml` goes to 1.7.

- **The SNMP interface table is a registered subject tree, so `sum by (index)`
  works** (#779). `zensight-common/registry/snmp.toml` registered a single
  rest-var catch-all, `{device}/{metric...}`. That pattern has **no literal
  chunks**, so the registry-driven family rule (#764) had nothing to name from
  and fell back to naming the family after the rest variable's *value* — which
  left the table index inside the metric name: `zensight_snmp_if_1_in_octets`
  and `zensight_snmp_if_2_in_octets` were two unrelated families, and no
  exporter-side rule could factor them back together. #769 had already made the
  index available as a label; this is the other half. Registry `snmp` moves
  **1.7 → 1.8** and registers the `ifTable`/`ifXTable` columns explicitly, one
  subject per column (`{device}/if/{index}/in_octets`, …, plus the `.rate`
  sibling the poller derives for every counter), which the generated parser
  tries ahead of the catch-all. The exported series becomes
  `zensight_snmp_if_in_octets_total{device="…",index="1"}`.

  **Not a wire change** — the poller publishes exactly the same keys it always
  did; only the exporters' reading of them moved. A Grafana panel or recording
  rule written against the old `zensight_snmp_if_<n>_<column>` names needs
  updating. `cpu/{index}/…`, `ip/{index}/…` and `storage/{index}/…` have the
  same shape and were deliberately left on the catch-all here; #783, below,
  finished the job in this same release and registered all three.

- **`zensight-conformance`: CI now asks a running fleet whether it obeys its
  own contract** (#744). A new `publish = false` workspace member that opens an
  un-namespaced observer session, runs `zenkey_fleet::run_doctor` — the same
  entry point `zenctl doctor` and the zengui doctor panel call — against a live
  deployment and turns the report into an exit code, plus
  `scripts/conformance-verify.sh` to stand that deployment up (isolated port
  17447, no containers, no privileges, the same `gen-configs.sh` every other
  run path uses) and a `conformance` job in `.forgejo/workflows/ci.yml` that
  runs both on every push.

  `cargo test --workspace` proves the code agrees with itself; `demo-smoke`
  proves the exporter path carries data. Neither could say whether what a
  *running* sensor puts on the wire agrees with the keyspace-v2 RFCs and with
  the registry TOMLs that same binary serves on `@rpc/…/introspect` — the
  served-vs-declared slice diff, `alive ⇒ callable` (RFC 04 §5), schema drift
  at field granularity, declared-vs-observed QoS, freshness against declared
  `ttl_s`, cardinality budgets. Those are properties of a deployment, and only
  a deployment can be asked.

  Exit codes are `zenkey_fleet::judgement_exit_code`'s, unmodified (RFC 13
  v1.24): `0` clean, `1` gated findings, `2` the run could not carry a verdict.
  An **empty roster is `2`, never `0`** — a harness that stood nothing up must
  not report a clean fleet.

  **`zenkey-fleet` is confined to this crate** and must not enter
  `zensight-common` or anything a sensor links: it is a bus-*explorer* engine
  and drags a full tokio, zenoh-ext, arc-swap, base64, ciborium and serde_json
  tree. Note also that its *package* license is Apache-2.0, not the MIT of the
  zenkey workspace root.

  **One check is excluded from the gate, in code, with the condition that lifts
  it: `field-new`, upstream zenkey#384.** `schema_drift`'s declared-field-path
  walker descends `properties` and not `oneOf`/`anyOf`, so every
  adjacently-tagged `TelemetryValue` (`#[serde(tag = "type", content =
  "value")]`, which schemars renders as a `oneOf` whose branches each require
  `type` and `value`) reports two phantom "never declared" warnings per
  telemetry key — 141 of them on a four-producer deployment. The served schema
  does declare both. Without the exclusion `--fail-on warning` is unusable, and
  a gate nobody can turn on protects nothing; `--deny field-new` re-arms it,
  which is how you find out whether #384 has landed. Nothing else is excluded —
  `info` findings (`admin-unreachable`, `storage-coverage`,
  `describe-missing`, the `{var...}` cardinality exemptions) simply sit below
  the severity floor, as facts about a deployment rather than defects in it.

  First real catch, reported and **not** excluded: the correlator's entities
  seed queryable answers `v1/@catalog/state/entity/*` storage-shaped with a bare
  `query.reply(key, payload)`, and session HLC timestamping applies to `put`,
  not to a queryable reply — so the seed carries no timestamp and cannot be
  LWW-ordered against a live sample (RFC 04 §4). It needs its own issue and a
  decision about *which* timestamp, so the CI deployment runs the correlator
  only under `CORRELATOR=1`; the check stays gated, and the correlator rejoins
  CI the day it stamps its replies. See `zensight-conformance/README.md`.

- **The Fleet view judges on RFC 13's four poles** (#746). `FleetStatus` had
  `InSync` / `Skew` / `Drift` / `Silent`, and `Silent` was doing two jobs. A
  host that is alive but answered no `introspect` might be an old build with no
  queryable, a broken queryable, one whose answer cannot be interpreted, or one
  the sweep never reached — and the view rendered all four identically.

  Rows now map onto `zenkey_fleet::Judgement`
  (`Established` / `NotEstablished` / `Unobservable` / `NotAsked`) through
  `FleetStatus::judgement()`, with six surface namings over the four poles:
  `in sync`, `version skew`, `drift`, `unreadable`, `no answer`, `not asked`.
  A tally line above the table counts all four.

  **The dangerous case is `NotAsked`.** A host missing because the sweep's
  reply bound cut the fan-in short rendered exactly like a fleet-wide failure
  to answer — and did so *more* readily the larger the fleet grew, which is
  backwards. Past the bound, replies are drained but not kept, so a missing
  producer may have answered and had its answer discarded; claiming "alive, and
  it answered nothing" about it is the false verdict RFC 09 §5.1 O4 forbids. A
  truncated sweep therefore reports `not asked` and names the bound; a whole
  sweep reports `no answer`.

  Neither unestablished pole borrows an answer's swatch (a new
  `theme::JUDGEMENT_UNOBSERVABLE`, and `STATUS_UNKNOWN` for `not asked`), and
  both sort between the findings and the clean rows — not verdicts, so they may
  not outrank one; not passing checks, so they may not sink below one.
  `not_asked_renders_distinguishably_from_every_other_pole` and
  `fleet_view_renders_not_asked_distinguishably_from_a_host_that_answered_nothing`
  pin it; without a test the distinction regresses.

  An unreadable slice moved too: it was `drift`, which is a claim about the
  content of a slice we managed to parse. It is `Unobservable` now, and the
  parse error — previously dropped on the floor — is the reason it carries.

- **The Fleet view runs on the upstream fleet engine** (#745). `view/fleet.rs`
  was hand-rolling `zenkey-fleet`'s core job: fan `introspect` across the
  fleet, parse each reply into a `RegistrySlice`, diff it against the
  compiled-in slice, classify the result. It now delegates all four.

  What that buys, beyond deleted code:

  - **The fan-in discipline is upstream's, in one place.** `RepeatingQuery`
    applies the RFC 05 §2.1 triple — target `All`, consolidation `None`,
    **attribution by the reply's own key**. The old sweep set `target(All)`
    but never `consolidation(None)`, so a producer that echoed the wildcard
    selector instead of replying on its own concrete key could collapse the
    fleet's replies to one.
  - **A bounded sweep that says what the bound cost.** The old fan-out was
    unbounded: a large fleet truncated at whatever the timeout caught, silently.
    Replies are now capped (`DEFAULT_MAX_REPLIES` = 4096, drained past the cap
    so the count is exact) and a sweep that dropped any renders a banner —
    "this inventory is a sample, not the fleet". Silent truncation gets *more*
    likely as the fleet grows, which is exactly backwards.
  - **Declared queriers.** The refresh path reuses two declared queriers
    instead of building a fresh `session.get` per producer per refresh, so the
    network keeps its routing state warm. They are replaced on (dis)connect: a
    querier belongs to the session it was declared on.
  - **One wildcard sweep, not one GET per compiled-in producer.**
    `v1/*/@rpc/*/introspect` plus `@catalog` by name (a `*` never matches a
    verbatim service origin, grammar property D4). The GUI no longer needs a
    compiled-in producer list to know who to ask, so a producer *newer than
    this build* now appears in the inventory instead of being unaskable.
  - **The comparison is `SliceSet::diff`**, per origin, against only the
    producers that origin serves — including the one-sided cases the view used
    to spell by hand. Findings arrive already rendered.

  `zenkey-fleet` is a **GUI-only** dependency: it pulls full tokio, zenoh-ext,
  arc-swap, base64, ciborium and serde_json, so it must never enter
  `zensight-common` or any crate a sensor links. The GUI does **not** open a
  session through it — `zenkey_fleet::open` builds an un-namespaced explorer
  session (RFC 09 §5) and refuses a `zenoh.namespace`, while the frontend's
  session is a production one from `zensight_common::session`;
  `Fleet::new(&session, "")` only borrows it.

- **Payload conformance verdicts, behind a `validate-json` feature on
  `zensight-common`** (#741). `SCHEMAS` — the RFC 08 §7 type table every
  producer serves on `describe` — had never been *used*: nothing validated a
  payload against it. `zensight_common::schema::verdict_for(type_name, &value)`
  does, with real draft-2020-12 validation and a compiled-validator cache keyed
  by schema hash.

  The answer is **three states, never a boolean** — "I did not check" must
  never render like "I checked and it passed". `NotValidated` says why:
  `FeatureOff` (built without the feature), `NoSchema` (the table was consulted
  and serves nothing for this type), `KindUnsupported` (a `protobuf`/`cdr`
  entry, whose decode *is* the check), `BadSchema`. `NoSchema` and `FeatureOff`
  are deliberately different answers and neither is `Valid`: one is "asked, and
  the type has none", the other is "nobody looked".

  The feature is **off by default and nothing turns it on yet.** `jsonschema` is
  real weight and a sensor has no use for it — a producer validating its own
  payload against its own derived schema is checking `schemars` against
  `schemars`. The consumer that has a use is a payload inspector, and **the GUI
  does not have one**: it decodes bytes into typed structs at `subscription.rs`
  and drops them, and no view renders a payload body. Building that surface is
  a feature in its own right rather than an upgrade consequence, so the GUI
  wiring #741 also asks for is **deferred**, with a note in
  `zensight-common/src/schema.rs` recording exactly what it needs.

- **`just demo-prometheus` and `just demo-otel`** (#751) — one command each for a
  working dashboard. Until now the exporters had **no run path at all**: zero
  mentions in the 442-line justfile, one service in `docker/docker-compose.yml`,
  and not one occurrence of the word "exporter" in `docs/DEPLOYMENT.md`. The
  stacks live in [`demo/`](demo/README.md): Prometheus v3.14.0 + Grafana 13.2.0
  with a provisioned datasource and dashboards, and `grafana/otel-lgtm` for the
  OTLP side. The exporter plays the Zenoh rendezvous the GUI plays under
  `just run`, so the demo is headless; the third-party stack runs on
  `network_mode: host` because the hub is a *loopback* listener a bridged
  container structurally cannot reach.
- **`scripts/demo-verify.sh`** and a `demo-smoke` CI job (#776) — sensor → Zenoh
  → exporter → `/metrics`, end to end, on isolated ports with no containers and
  no privileges. It validates every `# TYPE` token and rejects any series with a
  duplicate label name, which is exactly what would have caught the two bugs
  above on their first commit. `release.yml` now also `--help`-smokes the two
  exporter images, which were previously built and **pushed without ever being
  executed**.
- **`scripts/gen-configs.sh --exporters`** (#775) — emits the two exporter run
  configs into `.run/`, with `demo-max` enabling the OTel traces signal. The
  `traces` block is now spelled out in `configs/otel-exporter.json5` and pinned by
  a `shipped_config_spells_out_the_traces_flag` test, per that script's rule that
  a sed may only flip a key that really exists.

- **The ladder's bitrate cap is pinned end to end** (#504). A headless e2e test
  runs two rungs identical in geometry and framerate and far apart in
  `bitrate_kbps` alone, on per-pixel noise, and measures what a subscriber
  actually receives on each exact `<tier>` key. It measures **throughput, not
  frame size**, which is the correction the measurement itself forced: on input
  the encoder cannot compress further it does not shrink each frame — both rungs
  emit ~48 kB and ~61 kB per access unit — it *sheds frames*, which is exactly
  why the sensor pairs `RateControlMode::Bitrate` with `skip_frames(true)`. Over
  one window the thin rung delivered 3 access units to the fat rung's 12. The
  stats plane could not have answered this: the stats handle is per stream,
  shared by every open tier and the preview, and `{stream}/stats/kbps` has no
  tier chunk.

- **The tier ladder shapes its encoder, not just its numbers** (#509).
  parallax-pipeline exposes thirteen `H264EncoderConfig` knobs and the sensor set
  five, so a tier could say what it delivers but nothing about how the encoder
  got there. `video.encoder` now sets `profile` / `complexity` / `usage_type` /
  `qp` / `max_slice_len` and a per-tier `gop_frames` override for every rung, and
  any tier's own `encoder` block overrides it field by field.
  - The knobs are **sensor-local by design**. `TierSpec` rides the catalogue and
    is a derived entry in the fleet-wide `describe` schema every producer serves
    (RFC 08 §7); putting encoder internals there would make an implementation
    detail a bus contract, and a viewer picks a tier by resolution, framerate and
    bitrate — never by entropy coder.
  - **Every knob ships unset**, and each is applied only when set, so an unset
    knob is OpenH264's own default by construction rather than by a copy of it
    that can drift. A default build is byte-for-byte what it was.
  - Two defaults were asked for and not shipped, with the reasons written into
    the docs. `complexity` is documented as the answer to a firing
    `encoder_overrun` — cheaper than dropping resolution, invisible to the
    receiver — rather than given a value nobody measured. And `max_slice_len` is
    documented as **not paying off yet**: MTU-sized NALs limit fragmentation
    loss to one slice, but this sensor publishes a whole access unit as one
    best-effort Zenoh sample, so a lost sample costs the whole AU however it was
    sliced. It is wired and tested so it is ready for a downstream RTP/WebRTC
    payloader; it is off until there is one.
  - All three H.264 profiles are verified to decode through the GUI's own
    OpenH264 path, so an operator can set any of them without discovering that
    the project's own viewer cannot read the result.

- **The encoder says when the bitrate cap is biting** (#510). parallax-pipeline
  0.6 hands out an `EncoderStatsHandle` — cloned before `Executor::start()` like
  every other live handle — and with it the one number this sensor could never
  compute for itself: `frames_dropped_by_rc`, the frames OpenH264 swallowed
  rather than overshoot a tier's bitrate. `skip_frames(true)` has been set since
  the rate-control mode was chosen precisely so that could happen, and nothing
  counted it. It now rides `{stream}/stats/rc_drops` as a counter, folded from
  each open tier's handle by the session actor on its existing 1 Hz tick and
  summed per stream like every other stat there. The fold is a *delta*, not an
  absolute: a tier switch hands the stream a fresh handle that restarts at zero,
  and a published counter must not walk backwards.
  - It is **disjoint from `drops` by construction**: the encoder numbers its
    output from its own emitted-frame count, so a frame it swallows never
    reaches the sink to be counted there at all — a split #692 only sharpens,
    by making `drops` the sink's own counter rather than an egress inference.
    `drops` remains the sink shedding under a
    slow consumer; `rc_drops` is the cap. The point is **omitted, not zeroed**,
    for RTSP passthrough and preview-only streams, which have no rate control —
    a `0` there would read as "the cap is not biting" when the truth is "there
    is no cap".
  - `fps` and `kbps` deliberately keep their published-plane meaning rather than
    moving to the encoder's `bytes_encoded`: they count what actually crossed
    Zenoh, injected parameter sets included and sink-shed frames excluded, and a
    passthrough camera has no encoder to ask. `encode_ms` likewise keeps its
    `TimedElement` mean — the handle's `last_encode_ns` is one sample of the
    inner encode call, and the `encoder_overrun` rule needs an interval mean of
    the whole `process()`. The two are now pinned to agree on the denominator.
  - The GUI's live tile appends `· capped` when the counter is *growing*
    between ticks — the absolute value only says the cap bit at some point.

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

- **`{stream}/stats/drops` is the sink's own counter, not an inference**
  (#692). It is read from `AppSinkHandle::stats().total_dropped`, folded as a
  delta per profile incarnation on the actor's existing 1 Hz tick. The
  sequence-gap inference at egress is deleted.

  This is not a new definition — it is the one already written down. Three
  places in the tree (`stats.rs`, `docs/streams.md`,
  `examples/media_loss_probe.rs`) all described `drops` as *"the `AppSink`
  shedding under a slow consumer"*, and it was a proxy for that number. A worse
  one: blinded across every RTSP reconnect by the DISCONT reset, absent on the
  preview path, and structurally **zero** on RTSP passthrough, whose
  `RtspSession` never stamps `Metadata::sequence` at all. Existing series may
  step up; they cannot step down.

  **What the issue asked for could not be built, and that is written down
  rather than quietly dropped.** #692 wanted the metric sourced from the
  pipeline bus's `MessageKind::Qos`, plus new `qos_proportion`, `jitter_ms` and
  `latency_ms` subjects. Checked against the pinned `parallax-pipeline 0.8.0`:
  QoS reaches the bus only from a sink's `take_upstream_event()`, and the only
  implementors are `AppVideoSink` and `AutoVideoSink` — every profile here ends
  in `AppSink`, which does not override it, so there is **no origin**. Only
  `RtpJitterBuffer` and `AutoVideoSink` declare an `Element::latency()`, and
  neither is in any graph here, so `query_latency()` is `None` and
  `LatencyChanged` is never posted. `Throttle::stats()` is unreachable on a
  running pipeline and link policies expose no counters. The bus is
  deliberately *not* attached: it would carry `Error` on a different schedule
  from the `EndReason::Error` #691 already types, which is two orderings for
  one event. All of it, with file:line evidence and the four conditions that
  would reopen it, is in the new
  `zensight-sensor-parallax/docs/qos-and-latency.md`, on the `qos-express.md`
  precedent.

- **`{stream}/stats/sink_queue`** (#692) — the deepest `AppSink` backlog across
  a stream's open profiles, sampled each tick (0..`SINK_QUEUE`). The queue that
  *precedes* shedding, where `drops` only says it already happened. Its evidence
  is asymmetric and the registry says so: a reading at the cap proves a backlog
  that survived a whole second, a `0` proves nothing.

  Worth knowing, and now measured rather than assumed: because every link is
  `LinkPolicy::Block` with a shallow channel, shedding does not begin until the
  *entire* chain has saturated — source, convert, scale, throttle, encoder,
  sink. On an 8 fps source that is **~3 seconds**, which is also how long a real
  stall takes to reach `stats/drops`.

- **`stream_degraded` alert** (#692) — fires when the graph shed more than ~9 %
  of what it produced over one stats interval. That ratio is `QosEvent`'s own
  `(processed + dropped) / processed`, computed from the sink's counters at the
  one place we can compute it. **Windowed**, unlike `encoder_overrun`'s all-time
  tail: a cumulative ratio could never clear, and one bad thirty seconds at open
  would hold the alert firing for the life of the stream. It sits beside the
  overrun rule rather than replacing it — overrun is the encoder missing its
  budget and fires *before* anything is lost; this is frames produced fine and
  then thrown away downstream.

  Registry `parallax` 1.8 → **1.9**: one subject, `registry.lock` regenerated,
  and the `drops` description rewritten because the widening to the preview path
  is a contract change the compat lock structurally cannot catch (it pins path,
  class and type name, not payload meaning).

- **`StoppableSource` is gone — the engine grew the switch it worked around**
  (#709). Every synchronous source the parallax sensor built was wrapped in a
  `StoppableSource` whose `StopHandle` flipped the next `produce()` to EOS. The
  wrapper existed for three stated reasons, and re-checked against the pinned
  `parallax-pipeline 0.8.0` **all three are false**: the executor does not run
  source loops on blocking threads (a synchronous `Source` is driven inline
  inside an ordinary tokio task, `element/traits.rs:2671`), it does not ignore
  downstream channel closure, and `abort()` alone *can* stop a live source — it
  raises the cooperative flag itself (`unified_executor.rs:974`), which the
  executor's own source loop polls at the top of every iteration
  (`:3861-3871`), broadcasting EOS and draining downstream with exactly the
  one-frame-period bound the wrapper claimed for itself.

  So the wrapper reimplemented, in eleven forwarding methods, a check the engine
  performs one layer down. That forwarding was never free: it is a standing
  hazard whose own comment says so, because a method upstream adds and we forget
  to forward is silently answered by the wrapper *instead of* the source. That
  has already cost us once — `set_output_budget` in #689, which is how an
  encoder sizes its output arena.

  Teardown now calls `PipelineHandle::stop()` on the handle the profile already
  owns. `stop()` borrows, so `Drop` can call it too, and **no `Stopper` is
  needed**: `stopper()` exists for callers whose handle `wait()` has consumed,
  which this one never is. The `stop()`-then-`abort()` order is kept and is not
  ceremony — `stop()` lets a source loop end and drop its device, and the
  exclusive-source tier switch depends on the outgoing tier releasing a V4L2
  device before the incoming tier opens it, which asynchronous cancellation
  cannot promise.

  The three `stop_source()` calls on the failed-open paths are deleted rather
  than translated: all three run *before* `executor().start()` has returned a
  handle, so no source task exists and the elements are simply dropped. The one
  post-spawn failure inside `start()` (`pipeline.activate()`) already raises the
  flag itself, through `TerminalOutcome::fail` → `record` → `shutdown.begin()`.

- **The `zenoh::open` CI guard greps more than one spelling** (#789). It matched
  the literal `zenoh::open(`, so `zenkey_fleet::open()` / `open_with_config()` —
  reachable since #745 put `zenkey-fleet` in the GUI's dependency tree — walked
  straight past it. Neither crate does it today (`Fleet::new(&session, base)`
  only *borrows* a session, which is how upstream's own `zengui` works), so this
  was never a live violation; it was a guard that no longer covered the ways a
  session can be opened, which is worse than it sounds, because the guard's
  value is making the rule unbreakable by accident.

  It now matches any lowercase-module `::open(` / `::open_*(` — `File::open` and
  every other type-associated constructor is capitalised and does not match —
  minus an explicit allowlist of the sanctioned entry points
  (`session::{open_session,connect,build_config}`), so *calling* the wrapper is
  recognised as the behaviour the guard exists to produce. The step now says in
  place that it is spelling-based and must grow.

- **`just test-ui`, and #687 is broader than #687 said.** `cargo test -p zensight`
  segfaults on a headless Linux box with Mesa installed, printing nothing at all
  — the process dies before libtest writes a result line. It is lavapipe:
  `iced_test::simulator` stands up a real wgpu device, wgpu picks Vulkan, a
  GPU-less host resolves that to Mesa's software Vulkan, and many tests doing it
  at once crash inside the loader.

  The issue and `zensight/docs/testing.md` both recorded this as a **`ui_tests`**
  problem. It is not. The crate's own **lib** tests take the same path and crash
  *more* often — measured on `master` at `3f8083b`:

  | target | default | `WGPU_BACKEND=gl` |
  |---|---|---|
  | `--test ui_tests` | 6 crashes / 40 runs | 0 / 40 |
  | `--lib` | **3 crashes / 10 runs** | 0 / 10 |

  Which is why the recipe is `-p zensight` and not `--test ui_tests`: one that
  covered half the affected targets would send the next person chasing a phantom
  in the other half — as it did here, during this very change.

  Still deliberately not `.cargo/config.toml`'s `[env]`, which would downgrade
  the real GUI's renderer too. CI is unaffected — the runner image ships no
  Vulkan ICD — so a red `test` job is not this.

- **The `zenkey-fleet` boundary is stated as an invariant, not as a count**
  (#792). `CLAUDE.md` and `zensight-conformance/{Cargo.toml,README.md}` all said
  `zensight-conformance` was the **only** crate that may link `zenkey-fleet`.
  #745 falsified that in the same wave by rebuilding the fleet view on the same
  engine. The rule that matters is unchanged and is now what all three say: the
  engine must never enter `zensight-common` **or any crate a sensor links**. Two
  consumer-side members link it, which is what the rule permits.

- **Decision recorded: the `@media` plane keeps `express` off** (#733). parallax
  0.8's `ZenohSink::media` applies express *on* to the `frame` profile
  ("a stale frame is worthless"), while zenkey RFC v1.26 M1 removed it from that
  profile ("batching engages only under back-pressure, so express is a no-op on
  an unsaturated link and spends per-message overhead exactly when a `drop`
  profile should be shedding"). We were already on the newer rule —
  `QosClass::express` returns `false` for every class — so nothing changes; the
  parallax sensor keeps publishing through `RawMediaPublisher` and does not
  adopt `ZenohSink::media`. The reasoning is written down in
  `zensight-sensor-parallax/docs/qos-express.md` and the behaviour is pinned by
  a named `express_is_off_for_every_class` test rather than by an assertion
  buried inside two others, so it does not get "fixed" toward parallax's table.

- **The executor preset comes from parallax** (#732, closes #693). `executor()`
  built a `UnifiedExecutorConfig` around a local `CHANNEL_CAPACITY = 4` whose
  own comment admitted "the reason for this is probably gone; the cap is kept
  until measured" — it was a workaround for a `JpegEncoder` arena-vs-channel
  collision that parallax 0.7 fixed with `set_output_budget`. 0.8 ships the
  number *and* the reasoning as `ExecutorConfig::live_video()`
  (`SchedulingMode::Async`, `channel_capacity: 4`, `shed_fatal_after: None`), so
  the constant and the stale rationale are replaced by the preset. Same values,
  same behaviour; the engine that owns both the queue and the arenas now owns
  the number too.

- **The hand-rolled Annex-B helpers are parallax's now** (#730, closes #708).
  `zensight-sensor-parallax/src/annexb.rs` was 230 lines of start-code scanning
  and an extract/cache/prepend dance the egress drove by hand; parallax 0.8
  ships all of it in `parallax::codec::annexb`, compiled unconditionally (that
  module deliberately links no codec, so "is this a keyframe" needs no encoder)
  and **codec-aware** — `is_entry_point`/`has_param_sets` take a `NalCodec` and
  answer correctly for H.265, where our `& 0x1F` on a two-byte NAL header
  returned nonsense. `ParamSetCache::prepare` replaces the whole loop and
  borrows rather than copies for every delta frame and every keyframe that
  already carries its sets, so a *repaired* keyframe now copies once where it
  used to copy twice. Our module keeps one helper with no upstream equivalent,
  `coded_slice_count`, reimplemented over upstream's scanner. No wire change.
  `annexb::h264_profile_level_id` is re-exported for #707 but deliberately not
  put on the stream catalogue — see below.

- **parallax-pipeline 0.8.0 → 0.9.0**, the release in which the engine grew an
  application: `parallax-player` and `parallax-iced` are new crates upstream,
  and the player is what found most of what 0.9.0 fixes — A/V synchronization
  anchored on two independent guesses, a flushing seek that could deadlock a
  paced pipeline, a sink reporting what it *offered* as what it *sent*, and a
  hidden window that dragged a whole pipeline into a low-rate equilibrium it
  never left. None of the seven upstream breaks reaches us: the plugin ABI
  (12 → 14) matters only to out-of-tree plugins and we ship none; the renamed
  `ZenohSinkHandle` counters, the `Mp4SeekPoint` time base, `MkvDemux`'s seek
  report, the audio sinks' out-of-segment drop and `with_loop` all belong to
  elements this sensor does not build — it terminates in an `AppSink` and
  sources from V4L2, RTSP or the test pattern. Verified rather than assumed, on
  the published 0.9.0: `cargo check --all-targets` on the sensor, the `h264` GUI
  feature, and the sensor's 87 tests.

- **parallax-pipeline 0.7.0 → 0.8.0** (#727), and the pin is now a single
  `[workspace.dependencies]` entry so the sensor that *encodes* and the `h264`
  GUI feature that *decodes* cannot drift onto two versions of the same
  bitstream contract. No source change was required: `Source`, `AsyncSource`
  and `Element` are method-for-method identical to 0.7, so the hand-written
  `StoppableSource`/`TimedElement` forwarding wrappers — the silent-breakage
  hazard that bit the 0.6 → 0.7 bump — still cover every method. (`StoppableSource`
  did not survive the release: #709, below, deleted it once the pinned 0.8.0 was
  checked rather than assumed.) 0.8's new
  defaulted method (`finish`, the terminal goodbye) landed on `Sink`/`AsyncSink`
  only, and we wrap neither. The three upstream breaks all miss us: we never
  construct `Metadata` literally (so its new public `coded` field is
  irrelevant), we never compare an `EncoderStats` (so its lost `Eq` is), and
  `RtspSrc` reconnecting by default is what #731 wants anyway.

- **The conditional-subject ledger is a real file now** (#739). RFC 08 §6.1
  requires every registered subject to be served by the build that ships it,
  and the exemption for a genuinely gated subject used to live as a
  `CONDITIONAL_FAMILIES` const in each sensor's `tests/registry_conformance.rs`
  — because the registry TOML has no `feature`/`when` field to say so in the
  slice itself. zenkey-build 0.7 adds that field's stand-in, so the fact now
  lives in `zensight-common/registry/conditional.lock`, and **zenkey-build
  fails the build** if a line names no live registry subject — a build error
  rather than a test failure, firing even for a producer with no conformance
  test. The ledger is two lines (netlink's eBPF-gated connect-latency
  percentiles) for the whole workspace, and the file's header explains why that
  is correct rather than an oversight: a gated *procedure* is declared
  unconditionally and answers `error/gated` / `error/unsupported`, so it needs
  no exemption, and netring's detector features widen the value space of
  `anomaly/{kind}/total` rather than adding subjects. Only a gauge with no
  honest reading needs excusing.
- **`deprecated.lock` documents `kind = "procedure"`** (#740). zenkey 0.7 /
  RFC 08 v1.26 extended `[[deprecated]]` from subjects to procedures, with a
  three-field ledger line `<kind>\t<producer>\t<path>`. Purely additive:
  the 18 shipped two-field lines still parse as `kind = subject` and nothing
  migrated. The ledger's header and `docs/KEYSPACE.md` now record that **kind
  is part of identity** — retiring a subject never releases a procedure of the
  same name — which matters concretely, because `parallax` has a `streams`
  procedure beside stream-shaped subjects and `@catalog` has
  `names`/`describe`/`introspect` beside `entity`/`alias`.

- **The cross-producer key expressions come from zenkey now, not from string
  literals** (#742). zenkey 0.7 added `selector::common_family(scope, family)`
  — the `*`-producer complement to the generated per-producer
  `Family::selector(scope)` — which retires the hand-spelled
  `all_health_wildcard`, `all_alerts_wildcard`, `all_name_evidence_wildcard`
  and the tail of `origin_alerts_wildcard`. The bytes are unchanged and a test
  pins that. Every expression still hand-spelled in `keyexpr.rs` now carries a
  rationale written **against 0.7** rather than against the version that first
  justified it — a stale rationale is worse than none — and
  `zensight-common/docs/keyspace-helpers.md` carries the same table. The
  focus-mode builders' `format!` fallback arms are a silent-routing hazard (a
  narrowing of `RemoteOrigin::parse` would quietly send every focus-mode
  subscription down the string path), so a new test pins that the typed and
  hand-spelled arms agree for a legal origin.
- **A non-ULID event id is now a publish error** (#742). RFC 04 §1.3 requires
  the trailing chunk of `events/<producer>/<subject…>/<id>` to be a
  time-sortable ULID, key-encoded lowercase, and that is the events class's
  only ordering guarantee. A non-ULID id can still be a perfectly legal
  *chunk*, so it used to mint a key the grammar accepts and the guarantee
  silently does not hold for. `EventPublisher` routes the id through zenkey
  0.7's `slug::ulid_slug`, so a producer bug surfaces as an error naming the
  RFC. Uppercase ULIDs (the `ulid` crate's own rendering) are key-encoded, not
  refused.

- **`zenoh` and `zenoh-ext` 1.9 → 1.10, workspace-wide** (#734). 17 crates take
  `zenoh`, four take `zenoh-ext`; 27 lockfile packages moved together. **The
  wire is compatible in both directions** — `zenoh-protocol`'s `VERSION` stays
  `0x09`, so a 1.9 sensor and a 1.10 frontend (or the reverse) open a session
  and exchange data normally, and a fleet may be rolled forward node by node.
  The two wire-format changes 1.10 makes are both to *non-mandatory*
  extensions, which a peer that does not recognise them skips rather than
  rejecting: the new timestamp-instrumentation stack (`0x7`, off by default)
  and the SHM handshake probe, which moved from a `Z64` to a `ZBuf` encoding
  (`shared-memory` is not enabled in this workspace, so it does not arise
  here — a mixed-version fleet that *does* enable SHM loses the SHM
  optimisation across a version boundary, not the session).
  No ZenSight source changed: all nine zenoh config-key paths
  `zensight-common/src/session.rs` writes still exist under the same names
  (`mode`, `namespace`, `connect/endpoints`, `listen/endpoints`,
  `timestamping/enabled`, `scouting/{multicast,gossip}/enabled`, and the three
  `transport/link/tls/*` keys), `timestamping/enabled` still defaults to
  router-only (`{router: true, peer: false, client: false}`) so the
  unconditional insert stays load-bearing for every peer-mode sensor, and both
  scouting switches still default *on*, which is what the unset case relies on.
  In `zenoh-ext`, `RecoveryConfig` gained a `retention_period` (default 1h) for
  publisher last-sample state and `CacheConfig::max_samples` became
  `NonZeroUsize`-checked — every call site here passes `1`, so the new
  zero-is-an-error path is unreachable. `just router-verify` /
  `just router-plugins` now pin `zenohd` and its plugins at 1.10.0: a
  version-mismatched storage plugin loads, logs one line and serves no storage.

- **parallax-pipeline 0.6.0 → 0.7.0** (#689). 175 upstream commits, and the
  `h264` GUI feature did not compile against it at all: `H264Decoder::decode`
  became private and `DecodedFrame` crate-internal when decoders became plain
  `Element`s, so the GUI's tile decoder now drives `Element::process` with its
  own `SharedArena` and takes geometry from `Metadata::video_dims()`. Pulls
  return a `Pulled { Buffer, Empty, Flushing, Ended(EndReason) }` instead of
  `Result<Option<Buffer>>`, which lets the egress loop tell a clean end from a
  failed one using the pipeline's own reason rather than inferring it from
  `Ok(None)` plus `is_eos()`. `AppSink` became async-only (`add_async_sink`),
  and `Source::handle_flow_signal`/`flow_policy` went away with the `Queue`
  element. Two upstream changes are not in its changelog's breaking list and
  were found by reading the source: `VideoConvert::convert` gained a
  `PlaneLayout` argument, and — invisible to the compiler — `Element` and
  `Source` grew defaulted methods that our `TimedElement`/`StoppableSource`
  wrappers silently swallowed, including `set_output_budget`, which is how the
  encoders size their output arenas; both wrappers now forward them (of the two,
  only `TimedElement` is still here — see #709). The
  `channel_capacity: 4` workaround is kept but its rationale is marked stale:
  0.7 fixes the arena-vs-channel collision it exists for, and re-deriving the
  number is #693's, since the cap also bounds latency.

- **The parallax docs no longer promise live re-tuning that no build serves**
  (#504). `README.md` and a whole `docs/streams.md` section described
  bitrate/GOP/framerate/preview-quality control running "on the pipeline's
  control handles — no teardown". The handles are real and cloned before the
  executor starts, but the session actor drives exactly one of them,
  `keyframe`; there is no `set_bitrate`, `set_max_height` or `set_rate` call
  anywhere in the crate. #494 designed the premise away — quality is which
  `<tier>` you subscribe to — and #513 settled that redefining a tier is
  config-only. The section is replaced by a short "why there is no live re-tune
  command" that says what was decided and what such a knob would actually cost,
  rather than a bare deletion: the claim has been written into this tree twice
  already. The `max_height` limitation bullet loses the same false clause (the
  cap is applied when the pipeline is built; nothing retargets the scaler).
  - `TierApplied` called itself "actual, not requested". That is true of
    `width`/`height`, which come from the built pipeline, and false of `fps`
    and `bitrate_kbps`, which are the configured targets read back out of the
    tier spec — and the GUI renders all four as the tile's real state. The doc
    now says which is which and points at where a real measurement lives.


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
- **The release pipeline starts the sensors image instead of asking it for
  help** (#472). The smoke test was `zensight-sensor-sysinfo --help`, which
  exits before a config is read: it proves the binary loads — worth having, and
  the reason it was written — and nothing else. Every way the image can be
  broken that is not a linker problem passed it, which is how the 0.10.1 image
  shipped five sensors whose `introspect` advertised artifact procedures they
  did not serve (#648): a release build warns rather than dying, so the check
  was green for three weeks. It now runs the real entrypoint — interface
  detection, config generation, all five binaries, the shared spawner under
  `FAIL_FAST=1` — for 25 s, and fails if the spawner returns on its own, if
  anything panics, or if any producer reports the RFC 08 §6.1 coverage warning.
  The correlator image keeps the `--help` check; it has no entrypoint to drive.

### Fixed

- **The catalog announced presence before it could answer** (#782). RFC 04 §5 is
  `alive ⇒ callable`, and the correlator broke it. `guard::acquire` claimed,
  elected **and** declared the owner `alive` token in one step, while `main`
  spawned every queryable after it — so between winning the election and
  declaring entities/names/introspect/describe/link/unlink, the catalog was on
  the roster and answered nothing. On a developer machine that window is
  microseconds and nothing had ever observed it; on a loaded runner it is wide
  enough for a conformance judge's `introspect` sweep to land inside, which is
  how it was finally seen. Election and presence are now two steps —
  `guard::acquire`, then `guard::declare_alive` — with the queryables in
  between, which is the discipline `zensight-sensor-core`'s runner has always
  had. Its `await_registry_coverage` helper does **not** fit here: it derives
  the serve-side spelling from *this host's* origin, which is right for a sensor
  and wrong for a producer on a service origin.
- **No queryable reply carried an HLC timestamp, so no state-class seed could
  be LWW-ordered** (#782). Zenoh's session HLC stamps a `put`. It does **not**
  stamp a queryable *reply*. `zensight-common/src/session.rs` forces
  `timestamping/enabled = true` on every session and a test pins it, so the
  reflex reading — "some path missed the setting" — was wrong; the setting was
  never the mechanism.

  RFC 04 §3.2 makes a producer answering a plain GET on a state selector a
  *storage* for the duration of that reply — the reply-key discipline is
  storage-shaped on purpose, so seeding works with or without a real one — and
  closes with the corollary that **an untimestamped sample cannot be
  reconciled**. Every seeded state sample this workspace ever served was
  therefore unorderable against its own successors, silently, because nothing
  in the tree reads `Sample::timestamp()`.

  It was not silent to `zenctl doctor --deep`, which reported `unstamped-state`
  at warning severity. The originally-reported instance was the correlator's
  entity seed, which is why `scripts/conformance-verify.sh` held the correlator
  back behind `CORRELATOR=1`. Chasing it found a **second, worse** one: the
  firing-alert seed in `zensight-sensor-core` has the identical defect and *is*
  in the default CI deployment. It only looked clean because a sensor that
  raises no alert inside the listen window replies zero samples — on a host with
  two disks over 90 % full it replies two, and the `conformance` job goes red.
  The gate was a coin flip on the runner's free space rather than on the branch
  under test.

  **The rule, and it is the deliverable rather than the patch:** *a queryable
  reply whose key is in the `state` class MUST carry a timestamp; a reply on an
  `@rpc` key MUST NOT.* An `@rpc` reply is a computed answer to a parameterised
  question, never the value at a key — no storage selector reaches it, nothing
  merges it into an LWW store, and stamping it would assert a reconcilability
  that does not exist. Only **two** of the ~35 `query.reply` sites in the
  workspace are state-class; the other 33 are unchanged, which is the point of
  stating a rule instead of shipping a list.

  Enforced as a type, not a convention. `served::serve_state_queryable` returns
  a `StateQueryable` whose `StateQuery` exposes **no** `reply()` — only
  `reply_state`, which takes a stamp — so an unstamped state seed is not
  something a caller can write through the seam. `serve_queryable` debug-panics
  (release-warns) on a state selector, the same policy as
  `check_registry_coverage`, so a producer's own tests fail on it; a CI grep
  tripwire covers the case that runs no test.

  The stamp is `seed_stamp(&session)` — the session HLC, **taken inside the same
  critical section as the snapshot it describes**. Stamping per reply instead is
  a resurrection bug: a seed loop snapshots and *then* replies, so a value
  updated mid-loop has its live `put` stamped `T` while the loop replies the
  stale snapshot value stamped `T' > T`, and LWW keeps the stale one.
  `a_seed_batch_never_out_stamps_a_later_put` is that invariant in its
  observable form. Deliberately **not** the payload's own write time:
  `HostEntity.last_updated` is wall-clock epoch millis, minting a `Timestamp`
  from it under the session's own id forges HLC state for that id, and it would
  turn `unstamped-state` (warning) into `stale-state` (**error**) for any
  entity older than its `ttl_s`.

  Costs nothing at the manifest: `.timestamp()` is `TimestampBuilderTrait`,
  which zenoh marks `#[zenoh_macros::internal_trait]` — a macro whose documented
  job is to *also* emit an inherent method — so neither the import nor zenoh's
  `internal` cargo feature is needed. `state_reply_builder_still_takes_a_timestamp`
  is a compile-level pin on that shim, so a future zenoh upgrade that drops it
  becomes a named build failure rather than a silent return to unstamped seeds.

  **The correlator now runs in the default conformance deployment.** Both
  `scripts/conformance-verify.sh` and `zensight-conformance/README.md` promised
  that would happen "the day the correlator stamps its seed replies", with no
  change to the gate — `unstamped-state` was never in `DEFAULT_EXCLUDED`, so
  nothing needed un-excluding. It also buys real coverage: `@catalog` is a
  *service* origin whose verbatim `@` chunk is a structurally different
  introspect key (RFC 08 §6, property D4), so running both halves covers both
  halves of the slice diff.

  **And it found a second defect, which is the gate doing its job.** Putting the
  correlator into the default conformance deployment made `zensight-conformance`
  fail on a loaded two-lane CI runner — not on the stamping, but on RFC 04 §5's
  `alive ⇒ callable`. `guard::acquire` did three things at once: claim, elect,
  and declare the owner `alive` token; `main` then spawned every queryable
  *after* it. So between winning the election and declaring
  `entities`/`names`/`introspect`/`describe`/`link`/`unlink`, the correlator was
  on the roster and answered nothing. On a developer machine that window is
  microseconds and nothing had ever observed it.

  `zensight-sensor-core`'s runner has always declared liveliness **last**, after
  `await_registry_coverage`, with the reasoning written at `DECLARATION_GRACE`
  (#648). The correlator is not a `SensorRunner`, so it never inherited the
  discipline. Election and presence are now two steps —
  `guard::acquire` then `guard::declare_alive` — with the queryables in between.

  That needed a new seam, because the obvious one does not fit:
  `await_registry_coverage` is **sensor-shaped**, deriving the serve-side
  spelling from *this host's* origin (`v1/h-…/@rpc/catalog/names`), while the
  catalog serves on the `@catalog` **service** origin. Pointed at the
  correlator it reported every procedure as unserved while the log said they
  were ready, and debug-panicked. `served::await_served` takes the concrete keys
  a producer declared and makes no assumption about how they were spelled;
  `registry_coverage_cannot_see_a_service_origin` pins why both exist.

  The effect is visible beyond the absence of a failure: before, the judge
  reported *"1 producer(s) judged"* or *"2"* from run to run on a constrained
  machine, because it observed the correlator inconsistently. After, it is 2
  every time.

  Wire-observable on the seed path, and nothing else: the payloads are
  byte-identical, no schema moves, no key moves, no registry entry changes
  (`catalog.toml` and `sysinfo.toml` already declare these families as
  `class = "state"` with a `ttl_s`, which is *why* the stamp is a MUST — the
  code caught up with the registry, not the other way round). All three in-tree
  consumers ignore `Sample::timestamp()` today, so nothing in ZenSight changes
  behaviour. An external consumer that implemented §3.2's merge honestly, and
  therefore had to special-case the unstamped seed, will now see it participate
  properly. No re-keying, no phantom state, no operator sweep.

- **The verify scripts blamed Zenoh discovery when a binary was simply missing**
  (#790). `BIN="${BINDIR:-target/${PROFILE}}"` is repo-relative, so anyone with
  `CARGO_TARGET_DIR` set — a shared build dir, a worktree that builds elsewhere,
  sccache — built to one place and was launched from another. `cargo build`
  reports success, so the script's own build step gave no hint; the run then
  waited 40 s for a roster that could never populate and failed with a paragraph
  about multicast that was true in general and irrelevant to what happened. It
  cost a wrong diagnosis before the cause was found.

  Three fixes, in a new `scripts/lib/verify.sh` shared by
  `conformance-verify.sh` and `demo-verify.sh`, because both had all three:

  - **`require_bins`** fails before starting anything, naming each missing path,
    the `BIN`/`BINDIR`/`PROFILE` it derived it from, and the `BINDIR=…` command
    that fixes it.
  - **The failure message can tell a dead child from an undiscovered one.** The
    discovery paragraph is the right diagnosis when the processes are alive and
    cannot find each other, and the wrong one when a process has already exited
    — which the preflight cannot catch, because a binary can exist and still die
    on a bad config or a busy port. Both scripts now ask `kill -0` first and
    print the dead child's log tail instead of the discovery text.
  - **The evidence outlives the exit trap.** Every failure message ended with
    `logs: $tmp/*.log`, pointing into a `mktemp -d` the `EXIT` trap had just
    deleted. Failures now keep the directory *and* inline the last 20 lines of
    each log, so the message is self-contained even if the directory is not.

- **`zenoh_e2e` saw a sibling test's sample under parallel load** (#785). Its
  four sessions were plain `zenoh::Config::default()` — peer mode with multicast
  scouting **and** gossip on — so they discovered each other, plus
  `publisher_registry.rs`'s, plus any live sensor on the host. Under
  `cargo test --workspace` the telemetry test would occasionally receive the
  CBOR test's sample and fail with `left: "cbor-device"`.

  The per-test key prefixes were never the bug: a `test_<nanos>/**` subscription
  cannot match a sibling's tree. The session layer was, and the fix was already
  written twice in the same directory — `publisher_registry.rs` and
  `router_storage.rs` both define an `isolated_config()` for exactly this
  reason, and `zenoh_e2e.rs` is the one file that never got it. Deliberately not
  `--test-threads=1`, which hides the coupling rather than removing it.

- **RTSP streams reconnect instead of dying** (#731, delivers most of #410). A
  dropped RTSP stream — a camera rebooting, a switch flapping, a Wi-Fi bridge
  dropping a packet — used to end the pipeline and close the stream, leaving the
  viewer to re-open by hand. parallax 0.8 makes `RtspSession` an `AsyncSource`
  whose `produce()` carries the retry loop, so the hand-written feeder task and
  its `AppSrc` are gone and the reconnect is the source's own: exponential
  backoff from 500 ms to a 30 s ceiling with full jitter, so a rack of cameras
  behind one switch does not retry in lockstep.

  `rtsp_connect_failed` changes meaning with it, and had to: it now fires on
  *sustained* failure — the initial connect, or a drop that outlasted the whole
  reconnect ladder — rather than on the first hiccup. That is what the rule was
  always named for. The ladder is deliberately **bounded** (8 attempts, ≈ 90 s)
  where upstream defaults to retrying forever, because forever would mean a
  camera that is gone never produces an error and the alert could never fire
  again.

  The first buffer after a reconnect carries `DISCONT`, and the egress re-arms
  on it: the cached SPS/PPS belong to the previous session, so it clears them
  and refills from the camera's own in-band sets rather than prepending stale
  geometry to the resumed stream's first keyframe.

- **`FrameMeta.dts_ns` is omitted when it equals `pts_ns`** (#728), as its own
  documentation always said ("if distinct from `pts_ns`") and as parallax's
  byte-compatible twin has always done. The producer wrote it unconditionally
  whenever the clock was set, so — our encoders emitting no B-frames — *every*
  frame on the `@media` plane carried a redundant copy of its own pts. Consumers
  that read `dts_ns.or(pts_ns)` (the documented shape) are unaffected. The
  attachment is pinned from now on against parallax's three canonical CBOR
  vectors, checked into `zensight-common/tests/fixtures/framemeta/` and
  round-tripped byte for byte — which settles #711 as **two types, one corpus**:
  `zensight-common` cannot depend on the video engine and parallax cannot depend
  on Zenoh, so the shared artifact is the bytes, not the type.

- **Duplicate label names are now structurally impossible** (#753). Both
  exporters assembled labels by pushing sources in order and de-duplicating
  against a hard-coded `source`/`protocol` list — so `disk/<dev>/io/*` emitted
  `device` twice, once from the semconv table and once from the sysinfo sensor's
  own labels, producing `zensight_system_disk_io{device="sda",device="sda",…}`.
  Prometheus rejects such a sample and a remote-write receiver rejects the whole
  batch; the OTel side had no de-duplication at all. There is now one merge in
  `zensight-common::exposition` with a stated precedence — structural, semconv,
  pattern vars, point labels, config defaults — where later never overwrites
  earlier and a shadowed candidate is dropped and counted rather than appended.
  A sensor can no longer forge `origin`, `source` or `protocol`.
- **Exporter documentation that produced an empty dashboard** (#761). Both
  READMEs recommended `key_expr: "zensight/v1/*/telemetry/**"`; since #466 the
  deployment base is the session *namespace*, not a key chunk, so that selector
  matches **nothing** — with a perfectly healthy session. The OTel README's
  `include_protocols: [… "syslog"]` matched nothing either (the token is
  `"logs"`), silently dropping every log record while `export_logs: true`. The
  `configs/*.json5` comments stating the default were wrong in the same way.

- **tcplife's byte and segment counters are real** (#681). They were hardcoded
  `0` in the kernel program — 0 of 196 records carried a non-zero counter on a
  host where `ss -ti` had numbers for the same sockets — because they live in
  `struct tcp_sock` rather than in the tracepoint's arguments, and CO-RE cannot
  reach them: its field relocations come from clang's
  `__builtin_preserve_access_index`, which rustc/bpf-linker do not emit. The
  sensor now resolves the five offsets from the running kernel's own BTF
  (`zensight-btf`, landed for #682) and injects them with
  `aya::EbpfLoader::set_global` before load, with `must_exist: true` so a future
  linker that stopped emitting the symbol fails loudly instead of silently
  zeroing every counter. Where BTF cannot supply them the counters stay zero
  and a new `counters_measured` flag says so, which is the distinction the wire
  could not previously express — the GUI renders a dash rather than a `0`. A
  host test parses the built object and performs the same symbol-and-size check
  `set_global` does at load, so the mechanism is guarded at `cargo test` time
  and needs no privilege; that test exists because the question of whether the
  symbol survives linking was first answered wrongly, off a stale build
  artifact.

- **The netlink eBPF connections channel now carries the kernel's own timestamp,
  and the retransmit table renders its address family** (#685, the remaining
  three of five defects). `ConnRecord.ts_ns` was stamped by the BPF program for
  every record and dropped in the userspace conversion, so
  `@rpc/netlink/connections` came back with no times on it at all — records
  could not be ordered or aged. It is not published raw: `bpf_ktime_get_ns()`
  counts from boot, so the sensor converts it against a `CLOCK_MONOTONIC`
  anchor (deliberately not `CLOCK_BOOTTIME`, which counts suspended time the
  kernel clock does not) and publishes `ts_unix_ms` in epoch milliseconds, the
  convention `DiscoveryReport` and `EventRecord` already use. The GUI grew a
  "closed" column, since a field nothing renders is not a fix.
  The `RETRANS` counter's `get`/`+1`/`insert` became a single `get_ptr_mut`
  load-add-store: the old form lost increments whenever two CPUs took a
  retransmit to the same peer at once, which softirq makes routine. It
  undercounts less rather than exactly — the residual race is documented rather
  than papered over, because closing it needs a per-CPU hash whose 4096×nproc
  cost is its own decision.
  And a fifth defect the issue did not list: `fam_digit` puts a **digit** (4 or
  6) on the wire while the GUI's `fam_label` matched raw `AF_*` constants (2 or
  10), so **every** retransmit row rendered its family as `?`. The test that
  should have caught it used `family: 2` — a value the sensor cannot emit — and
  asserted nothing about the label. Both functions now have unit tests, and the
  UI test asserts the rendered value.

- **Four small eBPF-frontier defects found during host validation** (#685). A
  latency window that had barely happened was published as a full one:
  `tokio::time::interval` fires its first tick immediately, so iteration one
  deltaed against a zeroed baseline and labelled microseconds as
  `window_secs`. It is now consumed as a priming read that seeds the baseline,
  so the first window published is a true interval. The GUI's side of the same
  report: two empty histograms rendered as a title and a Refresh button with a
  blank gap between them — reachable on any idle host, and indistinguishable
  from a broken panel. It now says the window had no samples, in its own words
  rather than the unavailable-collector copy, since "attached but quiet" and
  "cannot measure at all" are different problems. And `just ebpf=1 sysinfo`
  built an eBPF binary, wrote `collect.ebpf: true` into its config, and then
  ran it with no capabilities, because the recipe depended on `build configure`
  while netring and netlink depend on `caps`; sysinfo's eBPF capabilities moved
  into a `_sysinfo-caps` recipe that `just sysinfo` depends on and that is a
  no-op off an eBPF build, so the unprivileged path still never prompts for
  sudo. `scripts/gen-configs.sh` no longer claims `just configure` passes
  `--ebpf` only when the capabilities are held — it gates on toolchain
  detection alone.

- **netlink's eBPF offsets are checked against the running kernel, and one
  struct name was wrong** (#682). Two comments in the program crate pointed at
  "the `btf_offsets` test in `-ebpf-common`". There was no such test: netlink's
  eleven `SS_*` and three `RT_*` offsets were guarded by nothing, because they
  sat as private constants inside a `#[cfg(target_arch = "bpf")]` module in a
  crate with no lib target — invisible to a host test three ways over. They now
  live in `-ebpf-common` beside a real `btf_offsets_match_this_kernel`, ported
  from sysinfo's. The reader both crates use moved into a new dependency-free
  `zensight-btf`, taken as a dev-dependency so it can never reach the
  `bpfel-unknown-none` object, and hardened on the way: it cannot panic, which
  the old test-only version could, and which #681 will need when it parses BTF
  inside a running sensor. The naming defect: the comment credited
  `trace_event_raw_tcp_retransmit_skb`, saying the event "has its OWN struct
  rather than the shared template". That name is **not in this kernel's BTF at
  all** — `tcp:tcp_retransmit_skb` is a `DEFINE_EVENT` of the
  `tcp_event_sk_skb` class, so it resolves to
  `trace_event_raw_tcp_event_sk_skb`, which is the size 80 the comment quoted.
  The right struct had been read and the wrong name written down. Corrected as
  a **candidate list** rather than a different single name, because the hazard
  the episode actually reveals is a tracepoint changing event class, which
  renames the struct and moves every field at once; the test also asserts every
  field resolved against the *same* candidate, so a blend of two layouts fails
  instead of looking right.

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

- **netlink's eBPF tier needs `CAP_PERFMON`, not `CAP_NET_ADMIN`** (#114). Six
  places — README, both docs, the module doc comment, the feature comment in
  `Cargo.toml` — said `CAP_BPF + CAP_NET_ADMIN`, while the code, the shipped
  config and the systemd unit said `CAP_BPF + CAP_PERFMON`. The code was right:
  these are kprobe and tracepoint *tracing* programs, and `CAP_NET_ADMIN` gates
  networking program types (XDP, tc, cgroup/skb) that this never loads. netlink
  genuinely does need `CAP_NET_ADMIN` — for nftables, conntrack and WireGuard
  peer data — and the two collectors had been conflated.
  - `CAP_DAC_READ_SEARCH` was missing from netlink's story entirely, including
    from its systemd unit. aya resolves a tracepoint by reading
    `<tracefs>/events/<cat>/<name>/id` and `/sys/kernel/tracing` is `0700`, so
    without it netlink's two tracepoints fail to attach **while its kprobes
    succeed** — a half-attached module, which is a nastier failure than a clean
    refusal. The unit now documents it as an opt-in line, commented, with the
    same "reads any file on the host" warning `zensight-sensor-sysinfo.service`
    carries.
  - `docs/telemetry.md` presented the whole tier as working. It now carries the
    host-validation result: which facets are trustworthy, which one is not, and
    the `perf_event_paranoid=3` trap (#683).

- **netlink's registry described two reply types that did not exist** (#114).
  `@rpc/netlink/retransmits` and `.../connections` were declared as
  `Vec<RetransmitRecord>` and `Vec<ConnectionRecord>`; **neither Rust type
  existed** — the sensor served `Vec<RetransRecord>` and `Vec<ConnView>` — and
  `describe` (RFC 08 §7) further reported the tcplife payload as "conntrack
  records", a different subsystem entirely. Same class as #513's phantom command
  and #479's phantom payload type.
  - Fixed by moving the two types into `zensight-common::query_detail` under the
    names the registry already used, with real derived schemas replacing the
    summary stubs. That is this repo's own rule — *"when adding a procedure, put
    its reply type in `zensight-common`"*, the precedent `LatencyReport` set
    under #469 — and it is also the only direction available: `zenkey-build`
    treats a changed `reply` on an existing path as an incompatible edit, and the
    `[[deprecated]]` escape is gated to subjects, so renaming the registry would
    have meant shipping `retransmits2`.
  - `registry.lock`, `types.toml` and the wire JSON are all **unchanged** —
    verified by `cargo build -p zensight-common`, whose build script enforces the
    compat lock. The GUI's two hand-written mirror structs are deleted in favour
    of the shared types, which is the drift this was always going to cause.

- **The documented eBPF capability set is not sufficient on Debian/Ubuntu**
  (#683). `CAP_BPF` + `CAP_PERFMON` + `CAP_DAC_READ_SEARCH` are necessary and
  not sufficient there: both distributions ship `kernel.perf_event_paranoid=3`,
  a patched level above upstream's maximum of 2, which restricts
  `perf_event_open` beyond what `CAP_PERFMON` relaxes. The programs **load**;
  every attach then fails `EACCES` — so the failure appears in the half of the
  process nobody is looking at, and reads as a capability problem it is not.
  Confirmed by changing nothing but the sysctl: at 3 the attach is denied, at 2
  the histograms come up, same binary and same caps. Root was never affected
  because `CAP_SYS_ADMIN` bypasses the check, which is why it survived the
  original bring-up. `just caps` now reads the sysctl and says so where the
  capabilities are granted, printing the confirmed-good value when it is fine;
  the requirement is in the sysinfo requirements table with the load-vs-attach
  distinction spelled out, and in the systemd unit's commented capability block.
  It is documented rather than applied automatically: lowering it relaxes
  `perf_event_open` for every unprivileged process on the host.

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

### Removed

- **`docker/configs/` — three config files that shipped nowhere** (#472). They
  were referenced by no Dockerfile, no compose file and no script: the sensors
  image copies the root `configs/*.json5`, and the per-component images expect a
  mounted `/etc/zensight`. Their only live consumer was a "shipped configs load
  strict" test in the logs crate, which now covers the two that really ship.
  They were not harmless: the SNMP one still carried pre-#559 `oid_names`, so
  #647 was written — and its changelog entry and crate reference worded — as a
  fix to a *shipped* config. `configs/snmp.json5`, the one the image actually
  carries, was already correct. Both references are corrected here.

### Decided

- **Rerun: an optional debugging backend, and the evaluation is closed** (#430,
  epic #415). `docs/plans/rerun/DECISION.md` is the terminal document of a
  21-issue evaluation. `zensight-rerun` stays in-tree, `publish = false`, out of
  the release train, and off unless someone runs the binary; it is **supported
  for bounded incident capture and replay** and for nothing else.

  Outcome 4 — a packaged, released backend — is refused rather than deferred,
  for four independent reasons: the adapter was never benchmarked at fleet rates
  (#426, not run), the viewer transport is unauthenticated, unencrypted and
  binds `0.0.0.0` by default, packaging would make Rerun's ~6-week breaking
  cadence our obligation, and structured events are modelled *worse* than in a
  backend we already ship (OTel's `LogRecord` carries body + severity +
  attributes in one record; Rerun needs `TextLog` **and** `AnyValues` on one
  path by producer-side convention, with nothing stopping them drifting).

  The maintenance number is not hypothetical: the pin is `=0.34.1` and upstream
  is 0.36.3 — two breaking minors, each with its own migration guide, in the
  seven weeks since we pinned. `zensight-rerun/Cargo.toml` now says in place
  that the pin does not move on a Renovate PR.

  Three operating rules are now written where a user will hit them
  (`zensight-rerun/README.md`): **`--bind 127.0.0.1` always** — the default
  exposes the web viewer *and* the gRPC proxy to the network, carrying
  hostnames, IPs, MACs, flow matrices and log lines; **`rerun rrd optimize`
  before storing or sharing** — 13x on our own recordings (~1.3 KiB/point live
  write becomes ~100 B/point, which is what killed the "untenable storage"
  reject-signal, and it doubles as the repair tool for a `kill -9`-truncated
  file); and **`--memory-limit`** on anything outliving a demo.

  What the evaluation is actually worth, beyond the verdict: it demonstrated
  that scrubbing backwards through a correlated incident across metrics, alerts,
  events and topology on one axis is a thing worth having. ZenSight already
  stores the samples. DECISION.md §6 records that as a native feature waiting to
  be specified rather than designing it.

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
