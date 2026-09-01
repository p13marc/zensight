# pve — configuration

`configs/pve.json5` is the shipped example. Unlike hostspec — whose empty
default set is a valid, conformance-green state — **this sensor cannot run
without being told an endpoint and a credential**, so the example is one an
operator edits. That is also why the assertion thresholds ship with real
values rather than switched off: whoever fills in the token is already reading
the file, and a monitoring tool whose checks all default to disabled asserts
nothing.

## The credential

```bash
pveum user add zensight@pve
pveum acl modify / --users zensight@pve --roles PVEAuditor
pveum user token add zensight@pve ro --privsep 0
```

`PVEAuditor` is read-only. `--privsep 0` gives the token the user's
permissions rather than an empty set. The token secret is shown **once**.

Put it in a root-0600 file and reference it:

```json5
token: "file:/etc/zensight/pve.token",
```

`file:` and `${ENV}` are the framework's secret indirection
(`zensight_sensor_core::resolve_secret`), resolved at startup so a missing
credential fails loudly at start rather than as a 401 every minute. The
`PVEAPIToken=` prefix is added if the resolved value does not carry it.

Under `DynamicUser=yes` — which the shipped systemd unit uses — the sensor
cannot read a root-0600 file directly, so the unit passes it through systemd's
credential store instead:

```ini
LoadCredential=pve.token:/etc/zensight/pve.token
```
```json5
token: "file:/run/credentials/zensight-sensor-pve.service/pve.token",
```

The file stays root-0600 on disk and the sensor gets a copy no other unit can
read.

## Keys

| Key | Default | Note |
|---|---|---|
| `pve.host` | — | a **host**, not a URL. A URL is refused at startup, by name. |
| `pve.port` | `8006` | |
| `pve.token` | — | see above |
| `pve.nodes` | `[]` | restrict to these PVE nodes; empty = all the API lists |
| `pve.source` | this machine's hostname | the reporting host every series, alert and evidence claim is filed under. Used to default to `pve.host`, which labelled everything `127.0.0.1` on the recommended deployment (#885). The vmid, storage and node are labels, not sources (#883) |
| `pve.poll_interval_secs` | `60` | runtime status — one `/cluster/resources` call |
| `pve.config_interval_secs` | `300` | guest configuration — one call per guest |
| `pve.backup_interval_secs` | `900` | vzdump tasks + stored volumes |
| `pve.timeout_secs` | `15` | **must be shorter than the poll interval** |
| `pve.max_concurrent` | `4` | concurrent API requests |
| `pve.accept_invalid_certs` | `false` | see below |
| `pve.evidence` | `true` | third-party identity claims about guests |
| `pve.alerts.*` | see [`assertions.md`](assertions.md) | |
| `pve.alerts.backup_job_failed` | `true` | grade a whole-job vzdump (`all 1`) once, rather than as a failure of every guest it covered (#880) |
| `pve.alerts.backup_task_max_age_secs` | `172800` (48 h) | how old a vzdump task may be and still be evidence about the last backup. The task query is bounded by rows, not time, so without this an ancient one-off wins forever. 0 disables |

## One-shot diagnosis

```bash
zensight-sensor-pve --config /etc/zensight/pve.json5 --diagnose
```

Asks the configured API everything the backup and storage rules depend on —
which pools will be listed, what each content listing returns, which volids
name no guest this sensor can read, how old each vzdump task is, and what the
guest disks sum to per pool — prints it in plain sentences, and exits. It is
read-only and **never opens a Zenoh session**: debugging a token should not
join a fleet.

### Three cadences, on purpose

Runtime status moves continuously, guest configuration moves on a human
timescale, and backups move once a night. Polling all three at the fastest of
those rates would be a monitoring sensor hammering the machine whose failure is
total — the same failure mode the SNMP sensor's per-device budget exists to
prevent one crate over.

The joined guest document therefore carries *this cycle's* status with
configuration that may be up to `config_interval_secs` old. When a config read
fails, the previous one is kept rather than dropped: a config read five minutes
ago is a far better answer than none, and dropping it would silently resolve
the very alerts it raised.

### The timeout is checked, not decorative

Startup refuses `timeout_secs >= poll_interval_secs`, naming both. A timeout
that cannot expire before the next tick is not a bound: a slow API just stalls
every cycle behind the previous one.

### `accept_invalid_certs`

A stock Proxmox install has a **self-signed** API certificate. Refusing
outright would push operators to something worse, so this flag exists — but it
means the endpoint is no longer authenticated, and an on-path attacker can feed
this sensor whatever hypervisor state they like. It is opt-in, logged at `warn`
on every start, and the better answer is to give the API a real certificate.

## Validation

Every problem is reported at once, not one per restart:

```
pve.host is empty; pve.token is empty — a read-only PVEAuditor API token is
required (use file:/path so the secret never enters this file);
pve.max_concurrent must be > 0
```

`shipped_config_parses` (in `src/config.rs`) loads `configs/pve.json5` and
validates it, so the example cannot rot unnoticed — nothing else reads it.
