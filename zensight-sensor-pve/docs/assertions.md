# pve — the ten assertions

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
