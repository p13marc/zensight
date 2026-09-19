# pve — the sixteen assertions

Every rule reconciles on **every sweep**: a condition that clears resolves, and
one whose input disappeared (a guest that was deleted) resolves too. Nothing
here needs a restart to forget.

All alerts carry `AlertKind::Expectation` and land on
`state/pve/alert/{alert_key}`. A guest-scoped alert's `source` is the **vmid**,
not the name — a rename would otherwise fork every series and every alert key
at the moment the operator most needs continuity. The name rides in the labels,
where changing it costs nothing.

## Guests

| Rule | Fires when | Severity | Labels |
|---|---|---|---|
| `guest-onboot-off` | a non-template guest has `onboot=0` | warning | `vmid` `name` `node` `kind` |
| `guest-not-running` | `onboot=1` but the guest is not running | critical | + `status` |
| `guest-nic-firewall-off` | a NIC has no `firewall=1` | warning | + `nic` |

Three details that decide whether these are useful or noise:

- **Absence is the finding.** Proxmox omits `onboot` and `firewall` from a
  config when they are 0. A parser that treats a missing key as "unknown, skip"
  reports nothing on exactly the guests that are misconfigured. The 2026-08-28
  audit's VM 140 had *neither key*.
- **`backup=0` on a disk is the opposite default.** vzdump includes a disk
  unless told not to, so an absent `backup` key means *included*. Getting these
  two backwards in the same parser is easy and total.
- **Templates are skipped entirely.** A template is a stamp, not a guest; it
  has no business being `onboot`, running, or firewalled, and asserting on one
  fires on every install that has ever made a template.
- **`guest-not-running` requires `onboot=1`.** A guest deliberately left off is
  not a fault, and `onboot` is the only machine-readable statement of intent
  Proxmox has. Without that gate, the rule would fire on every parked VM.
- **Nothing is graded for a guest this sweep cannot see** (#1132). See below —
  it is the fourth detail and it was missing.

### The sweep has to be entitled to its opinion

`/cluster/resources` on a node that has lost quorum **still answers**. It
reports the guests on the far side of the partition as `status: "unknown"`, and
`is_running()` is `status == "running"` — so a ninety-second corosync blip
fired a **critical** `guest-not-running` for every VM in the cluster, alongside
`cluster-not-quorate`, and none of them had stopped.

Two states hold every guest rule rather than grading on what the API happened
to say:

| State | What is held |
|---|---|
| `quorate == Some(false)` | **every** guest — a node without quorum is not entitled to an opinion about anything but itself, and Proxmox's own tooling refuses to act in that state |
| a node listed with `online == false` | that node's guests only; guests on nodes that answered are graded as usual in the same sweep |

A standalone node (`quorate: None`) is always observable: there is no quorum to
have or lose. But a `/cluster/status` read that **failed** is graded as
non-quorate and not as standalone — folding the two together would turn the
guard off exactly when the cluster API is the thing that is unwell.

**Holding is two halves, and the second is the one that is easy to miss.**
`grade` not emitting an alert is not enough: the poller reconciles every rule
every sweep, and a fleet-wide reconcile reads "did not fire" as "recovered".
So the three guest rules reconcile **per node**
(`reconcile_labeled(rule, "node", …)`), over the nodes this sweep could speak
for. A node that did not answer is skipped, and its guests keep their alerts.

The same reasoning applies to telemetry: `guest/{vmid}/running` is not
published at all for a held guest. A `0` there is this sensor turning "we
cannot see it" into "it stopped", which is the one claim the rest of that block
refuses to make.

This is SNMP's `device_answered` and BMC's `chassis.is_none()` guard, one API
over — the lesson both of those already paid for.

`alerts.exempt_vmids` excludes a guest from all three — the VM that is *meant*
to be off, or the one on a bridge with no firewall at all.

## Storage

| Rule | Fires when | Severity |
|---|---|---|
| `pool-usage` | `used/total ≥ alerts.pool_used_pct` (default 85) | warning, critical at ≥95% |
| `pool-overcommitted` | `allocated/total ≥ alerts.pool_overcommit_ratio` (default 1.0) | warning |

`allocated` is the sum of the declared sizes of the pool's volumes — what has
been *promised*. On a thin pool it is unrelated to `used` and it is the number
the audit needed: 990 GB against 937 GB of capacity, with usage showing 32 %
and nothing to reconfigure before it filled.

When the content listing cannot be read, `allocated_bytes` is **`None`, never
0**: zero would read as "nothing provisioned", which is the one wrong answer
this family can give.

### On a cluster: two kinds of pool

`/cluster/resources` lists a **shared** pool (NFS, Ceph, PBS) once per node —
every row is the same bytes, so it is asked once and kept once. A
**non-shared** pool (`local`, `local-lvm`: every node has them) is a
*different* pool on every node that happens to carry the same name, with its
own capacity, its own volumes and its own over-commitment. For a while the
sensor collapsed pools on the name alone: a three-node cluster kept one
`local-lvm` and dropped the other two, and the derived total then summed
*every* node's guest disks into the survivor — roughly N× too high, and a
false `pool-overcommitted` on a pool that was half empty.

Now the derived total for a non-shared pool counts only the guests on that
pool's node, and **every** non-shared pool carries the node in its key chunk
(`storage/<node>-<name>`), so two pools do not take turns overwriting one
document. vzdump tasks are asked of every node, not only the nodes the
(deduplicated) pool list happened to keep.

**The disambiguator is `shared`, a property of the pool — not "did this sweep
see the name twice"** (#1132). It was the latter for a release, which made the
KEY depend on which nodes answered: when node B dropped out of the cluster,
node A's `local-lvm` moved from `storage/pve1-local-lvm` to
`storage/local-lvm`, its series restarted under a new name, and the old state
document became an LWW ghost that nothing would ever overwrite again. A key
that changes shape with the weather is not a key.

The cost is that a single-node deployment's pools are `storage/pve-local`
rather than `storage/local`. That is the right trade: a name is either
disambiguated or it is not, and "not yet" is how the ghost got made.

### Reported vs derived (#881)

PVE surfaces a per-volume size for LVM-thin and ZFS and **nothing for a `dir`
storage** — so on the storage type the reference deployment actually runs, the
number this rule exists for was not reported at all, and summing the empty set
gave `Some(0)`: "nothing is provisioned", the exact opposite of the truth, and
a rule that could never fire.

It can still be *derived*, because the sensor already reads every guest's disk
lines and every disk names its storage. Where the plugin reports nothing, the
pool's `allocated` is the sum of the guest disks that live on it, and the
document and the gauges both say which it is:

| `allocated_source` | Meaning |
|---|---|
| `reported` | the plugin's own per-volume sizes, summed |
| `derived_from_guests` | summed from the guests' disk lines — a **floor** |

A derived total is a floor, not a measurement: a volume no guest currently
attaches (`unused<N>`) still occupies the pool and is deliberately excluded
from `provisioned_bytes`, and a disk with no `size=` (efidisk, TPM state)
contributes nothing. Absent both, `allocated_bytes` stays `None`. The two are
never conflated, and a dashboard comparing two pools can see which is which.

## Backups

| Rule | Fires when | Severity |
|---|---|---|
| `backup-failed` | a **recent, per-guest** vzdump task did not exit `OK`, and no newer volume supersedes it | critical |
| `backup-job-failed` | the last **whole-job** run (`all 1`) did not exit `OK` | critical |
| `backup-stale` | the newest stored dump is older than `alerts.backup_stale_secs` | warning |
| `backup-shrunk` | the newest dump is ≥ `alerts.backup_shrink_pct` smaller than the one before it | critical |

### The volumes are the evidence; the tasks say why (#880)

A job configured `all 1` covers every guest and therefore **names none**: PVE
records it as one task with an empty `id`, and the per-guest results exist only
inside the task log, as free text. The first deployment against a real API met
exactly that, and three things went wrong at once — every nightly task was
discarded, the sensor fell back to whatever one-off task happened to be tagged
with each vmid (on that fleet, a failure from six weeks earlier), and a template
the job explicitly excludes fired a critical about a backup it was never meant
to have.

So:

- **A whole-job run is graded once**, as one job, under `backup-job-failed`.
  Seven per-guest criticals for one job is not seven findings. Attributing it
  per guest would mean parsing the task log's free text; this sensor
  deliberately does not, because the stored volumes answer the question
  ("was *this guest* backed up?") without guessing at a log format.
- **Tasks have an age bound** — `alerts.backup_task_max_age_secs`, default 48 h.
  The task query is bounded by row count, not by time, so without it the oldest
  surviving one-off wins forever. A stale task is not evidence about last night.
- **A newer volume supersedes a failed task.** If a dump exists that is newer
  than the failure, the failure has already been overtaken by a run that worked.
- **Templates and `exempt_vmids` are skipped**, exactly as in the guest rules.
  A template is a stamp, not a guest.
- **`volumes` is `Option<u32>`.** `None` when no backup-capable pool could be
  listed at all — refused, failed, or nothing to ask. `0` says "this guest has
  no backups", which is a very different claim from "we could not look", and
  the reference deployment saw the second reported as the first.

Every refusal on this path is now logged at `warn` on the transition, once per
endpoint. Before, a 403 became an empty result with no log line at any level: a
sensor reporting a confident zero it had never been allowed to measure.

**`--diagnose`** asks the configured API everything these rules depend on —
which pools will be listed, what the content listing returns, which volids name
no guest, how old each task is, and what the guest disks sum to per pool — then
prints it and exits. It never opens a Zenoh session.

`backup-shrunk` is the rule that justifies reading backup *sizes* at all. "The
job exited 0" is what the mail notification already says; a dump that succeeds
and halves is a restore that will not work, and nothing else notices. The
baseline is the previous volume **in the store**, so a sensor restart cannot
turn a shrunk backup into the new normal.

`backup-stale` is the one assertion that defaults **off** (`0`). Backup cadence
is deployment policy; a wrong default is a nightly false positive. For a
nightly job, `93600` (26 h) leaves room for a slow run.

A vzdump task that is still *running* has no verdict and is skipped — not
counted as a failure.

## Cluster

| Rule | Fires when | Severity |
|---|---|---|
| `cluster-not-quorate` | `quorate` is explicitly false | critical |
| `replication-failed` | a replication job's last run failed | warning |

`quorate` is `None` on a standalone node — the commonest Proxmox install — and
the rule cannot fire there. Reporting "not quorate" for a single node would be
a permanent false positive on most of the installed base; there is no quorum to
lose.

When it **does** fire it is the only rule that fires: every guest rule is held
for the duration (#1132, see [Guests](#guests) above). That is the point of the
pairing — `cluster-not-quorate` is the fact, and a page of `guest-not-running`
criticals for VMs that never stopped is the noise it used to arrive with.

The HA resources in the cluster document are the rows
`/cluster/ha/status/current` marks `type: "service"`. That endpoint is a
**status feed**, not a resource list: its other rows are `quorum`, `lrm` and
`master`, and taking them wholesale (#1132) put three or four phantom services
per node in the document, none of which `ha-manager` would ever name.

## Nodes (#1141)

| Rule | Fires when | Severity | Labels |
|---|---|---|---|
| `node-rootfs-full` | the node's `/` is at or past `node_rootfs_ratio` (default 0.9) | critical | `node` |
| `node-load-high` | `load1 / cpus` is at or past `node_load_per_cpu` (default 4.0) | warning | `node` |
| `node-swapping` | swap in use is at or past `node_swap_ratio` (default 0.5) | warning | `node` |

The sensor reported every guest and every pool while **the hypervisor those
guests run on was invisible** — which is the first thing anyone looks at when a
guest is slow. `/nodes/{node}/status` is what it never read.

Three details that decide whether these are useful or noise:

- **`node-rootfs-full` is `/`, not a storage pool.** No pool's numbers contain
  it, so `pool-usage` could never have said this — and a full root filesystem
  stops PVE writing its own state, which is a more total failure than a full
  pool.
- **Load is graded per CPU.** A raw load average means different things on a
  4-core and a 64-core node, and a single fleet-wide threshold has to mean one
  thing. The default is deliberately high: a hypervisor is *supposed* to be
  busy.
- **A node with no swap never fires `node-swapping`.** `swap_ratio()` is `None`
  when the total is absent or zero, and a no-swap host is a deliberate
  configuration rather than 0 % used.

`loadavg` arrives from PVE as an array of **strings**. `as_f64` reads those as
`None`, so the client parses the string — not belt and braces, the only thing
that works, and a client that did not would publish no load at all while
looking like it had.

## Backup schedules (#1141)

| Rule | Fires when | Severity | Labels |
|---|---|---|---|
| `backup-job-overdue` | an **enabled** job's `next-run` is past by more than `backup_overdue_grace_secs` (default 3600) and nothing has run since | critical | `job` |

This is the assertion `backup-stale` cannot make. Staleness is measured against
a fixed age, so a job that was **switched off**, or whose schedule was edited
away, looks exactly like one that is merely young. A schedule says when it was
*due*.

- **A disabled job is never overdue.** It is switched off, which is a different
  thing to tell an operator, and firing on it would make every deliberately
  paused job a standing alert nobody can clear.
- **The calendar spec is not parsed here.** `next-run` is systemd's own
  evaluation of it, handed over by PVE. A spec this build evaluated differently
  would be a confident wrong answer about when a backup was due, so a release
  that does not report `next-run` is **not graded** — the schedule is still
  published, as a fact.
- **Something that ran after the due time clears it**, whatever the clock says.

## Ceph (#1141)

| Rule | Fires when | Severity |
|---|---|---|
| `ceph-health` | Ceph's own `health.status` is `HEALTH_WARN` or `HEALTH_ERR` | warning / critical, as Ceph rates it |

**Ceph's own enum, never our reading of the counters beside it** — the same
rule the BMC sensor follows about somebody else's hardware, and for the same
reason: Ceph knows what its numbers mean and we do not. The OSD, monitor and PG
counters ride in the document as context for the verdict, not as inputs to it.

Absent entirely on a cluster that does not run Ceph: the endpoint answers 501
or 404, which is a fact about the cluster rather than a failed poll, and
nothing at all is published — no zeroes, no `healthy: 0`. A cluster with no
Ceph is not a cluster with unhealthy Ceph.

`pgs_degraded` is summed over every state that is **not** `active+clean`, which
is the only reading that survives Ceph adding a state name.

## What is deliberately not asserted

- **Anything requiring a write.** No "restart this guest", no "enable onboot".
  See the crate docs.
- **Guest-internal state.** The hypervisor knows a guest is running; whether
  the service inside it works is what that guest's own sensors are for.
- **Backup *content*.** The sensor reads sizes and task outcomes. Whether a
  dump restores is a restore test, and claiming otherwise would be worse than
  silence.
- **HA state transitions.** The HA resources are published in the cluster
  document; nothing grades them yet, because on a fleet with no HA there is
  nothing to calibrate a rule against.
