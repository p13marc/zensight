# ZenSight exporter demos

Two one-command stacks that put real ZenSight telemetry into real dashboards.

```bash
just demo-prometheus   # exporter + Prometheus + Grafana
just demo-otel         # exporter + grafana/otel-lgtm (Collector · Prometheus · Tempo · Loki · Grafana)
just demo-stop         # tear either down
```

Both build what they need, generate their configs, start the full sensor set,
and print their URLs. Ctrl-C stops everything. The first run does a release
build and a container pull, so give it a few minutes.

| | Prometheus demo | OTel demo |
|---|---|---|
| Grafana | <http://127.0.0.1:3000> (opens on **ZenSight — Host overview**) | <http://127.0.0.1:3000> (Explore) |
| Prometheus | <http://127.0.0.1:9090> | <http://127.0.0.1:9090> (inside otel-lgtm) |
| Exporter | <http://127.0.0.1:9464/metrics> | pushes OTLP to `127.0.0.1:4317` |
| Images | `prom/prometheus:v3.14.0`, `grafana/grafana:13.2.0` | `grafana/otel-lgtm:0.11.14` (bundles Grafana **12.2.1**) |
| Pinning | by digest, tag in a comment beside it | by digest |

**They are mutually exclusive.** Both bind host TCP 3000 and 9090. Run one at a
time; `just demo-stop` clears either. If you already have your own Grafana on
3000 the stack fails to start with `bind: address already in use`, which is the
correct failure.

## Troubleshooting

### `/ready` says 503, and the dashboards are empty

This is the number one failure, and it is almost always **Zenoh discovery**.

`configs/{prometheus,otel}-exporter.json5` ship as `mode: "peer"` with `connect`
commented out. A peer with no explicit connect gets **multicast scouting on**
(`zensight-common/src/config.rs`) — but every ZenSight demo path turns multicast
**off** on purpose, because it is unreliable on hosts with a VPN or extra
interfaces, and on loopback it triggers a `CONNECTION_TO_SELF` error storm.

So an exporter started by hand against a `just run` hub finds nothing, silently,
forever. The `just demo-*` recipes set `ZENSIGHT_ZENOH_{LISTEN,CONNECT}` and
`ZENSIGHT_ZENOH_SCOUTING=false` explicitly, which is what makes them work. If you
are running the exporter yourself, you need the same:

```bash
ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:7447 ZENSIGHT_ZENOH_SCOUTING=false \
  target/release/zensight-exporter-prometheus --config .run/prometheus-exporter.json5
```

`/ready` returning 503 means exactly this: the exporter is healthy and has
received no telemetry. `/health` stays 200 — it answers "is the process up", not
"is data flowing".

### The Prometheus target is DOWN

The exporter is not listening. Check `curl -s 127.0.0.1:9464/metrics`. Note the
port: **9464, not 9090**. 9090 is Prometheus's own port — it used to be the
exporter's default too, which is why the old README told you to scrape
`localhost:9090`, i.e. Prometheus scraping itself.

### Grafana shows a dashboard with no data

If the target is `up` and `/metrics` has content, the panel may be one whose
sensor is not running: the provisioned set is **host overview**, **network
(netlink)** and **alerts & exporter**, and the netlink panels stay empty unless
`just demo-prometheus` actually started the netlink sensor.

Panels that are *parked* rather than empty live in
[`prometheus/dashboards-blocked/README.md`](prometheus/dashboards-blocked/README.md),
with the reason each one is parked. Netring is parked pending a verified scrape
(it needs `CAP_NET_RAW`); SNMP per-interface names became aggregatable in
registry 1.8 (#779) but no panel has been verified against a device yet.

### Verifying without any of this

```bash
just demo-verify
```

Runs sensor → Zenoh → exporter → `/metrics` on isolated ports with no
containers, no sudo, and no Grafana. If that passes and the demo does not, the
problem is in the stack, not the exporter.

## Networking: why the exporter runs on the host

The exporter runs as a **host process**; only Prometheus, Grafana and otel-lgtm
are containerised, all on `network_mode: host`. That is forced, not lazy.

The Zenoh rendezvous is `tcp/127.0.0.1:7447` — a **loopback** listener. A bridged
container reaches the host through its gateway address (`host.containers.internal`
under podman, `host.docker.internal` under docker), and a socket bound to
`127.0.0.1` does not answer there. A containerised exporter therefore cannot
reach the bus at all — and it fails by *hanging in reconnect*, not by erroring,
which is the worst possible way to debug a demo.

Making it work the other way would mean `just gui listen=tcp/0.0.0.0:7447` —
exposing the whole ZenSight bus on every interface to run a local demo. Not worth
it.

Host networking also means one addressing model, byte-identical under docker and
podman, with no magic hostname and no `extra_hosts: host-gateway`. `ports:` is
absent from both compose files because under `network_mode: host` it is a no-op
that only generates a warning.

**Linux only.** Host networking is a Linux feature, and the sensors are
Linux-only anyway (`/etc/machine-id`, journald, netlink).

### The exporter is the rendezvous

`just run` makes the **GUI** the Zenoh listener. These demos are headless, so the
**exporter** takes that role: it listens on the hub and the sensors connect to
it. Same topology, one fewer process, and no "start the GUI in another terminal
first".

If a hub is already up — you have `just run` going in another terminal — the
recipes detect it and attach as a spoke instead of fighting for the port.

## podman vs docker

- **podman is canonical for building images** (`just image`,
  `scripts/image-verify.sh`, and CI's `buildah` all use it).
- **`docker compose` is canonical for compose files**, which is what
  `docker/docker-compose.yml` already documents.

The recipes detect a front-end and accept `podman compose` / `podman-compose` for
hosts that have only those, but they prefer `docker compose` so the documented
command is the one that runs.

## Remote-write instead of scraping

The Prometheus exporter also supports **push** (`remote_write.rs`, off by
default). The demo's Prometheus already runs with
`--web.enable-remote-write-receiver` (the Prometheus 3.x **flag**; the old
`--enable-feature=remote-write-receiver` spelling is deprecated), so to try it:

1. In `.run/prometheus-exporter.json5`, set
   `remote_write: { enabled: true, url: "http://127.0.0.1:9090/api/v1/write" }`.
2. **Comment out the `zensight-exporter` scrape job** in
   `prometheus/prometheus.yml` — otherwise every series arrives twice, once
   pushed and once scraped.

The demo ships **pull** because it is the path most people will deploy. Push is a
documented one-liner, not a second code path.

## What the OTel demo shows, and what it does not

Grafana → Explore, then pick a datasource:

- **Prometheus** — the metrics, under semconv names (`system_cpu_utilization`, …)
- **Loki** — exported journald lines
- **Tempo** — alert-lifecycle spans, if you enabled traces

**Be clear-eyed about the traces.** ZenSight propagates no W3C trace context
across sensors, so these spans are **synthesized**: one parentless span per alert
firing→resolved transition, with ids hashed from the alert key and its firing
timestamp. There is no service graph and no waterfall. And **only a resolved
alert produces a span at all** — an alert that is still firing shows nothing, and
if the exporter starts mid-lifecycle the resolve yields nothing either.

It is genuinely useful for "how long was this alert firing" and for spotting
flap patterns. It is not distributed tracing, and a demo should not imply that it
is. `gen-configs.sh --exporters` turns it on for the `demo-max` profile;
`production` leaves it off.

## Where the pieces live

```
demo/
  prometheus/
    compose.yml                     Prometheus + Grafana, host network
    prometheus.yml                  scrape config (exporter on :9464)
    provisioning/datasources/       the ZenSight-Prometheus datasource (uid: zensight-prom)
    provisioning/dashboards/        the file provider
    dashboards/                     provisioned — these work today
    dashboards-blocked/             NOT provisioned, and why
  otel/
    compose.yml                     grafana/otel-lgtm, host network
```

The OTel demo ships **no dashboard**, by the same rule: this repo provisions no
panel it has not watched render, and the image's own Grafana is 12.2.1 with a
`uid: prometheus` datasource at `timeInterval: 60s`, so the Prometheus demo's
JSON cannot simply be reused. Explore is the documented path; `otel/compose.yml`
records what a future dashboard would have to match.

`dashboards-blocked/` is a **sibling** of `dashboards/`, not a child, because a
Grafana file provider walks subdirectories — a `blocked/` folder inside the
mounted path would be provisioned as a folder full of empty panels.

## The incident demo (#945)

`just demo-incident` is the one demo that shows what ZenSight does that a pile
of series does not.

**The setup.** Two synthetic hosts go on the bus — `pve01` hosting `vm101` — and
`vm101` has a firing alert. For the first thirty seconds that alert is
*unexplained*: nothing is down, so nothing can be its cause. Then `pve01`'s
liveliness token drops, and the catalog re-files the guest's alert as a
`symptom_of` `pve01`.

**The comparison is the demo**, and it needs two windows:

```bash
just demo-incident                                    # terminal 1
ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:17450 \
  ZENSIGHT_ZENOH_SCOUTING=false just gui              # terminal 2 (needs a display)
just demo-prometheus                                  # terminal 3, optional
```

The GUI's incident view shows the root-cause candidate and the alert it
explains. Grafana, given the same series, shows the same lines going flat and
**no relationship at all** — because a relationship is not a series, and that is
the entire argument for the catalog.

**Nothing is mocked.** The fault publisher puts real `HostEvidence`,
`RelationshipEvidence` and `Alert` documents on the wire in their shipped shapes
and declares real liveliness tokens; a real correlator subscribes them, runs the
real union-find, resolves the real edge and publishes the real incident. There
is no path in the demo that can produce an incident a fleet could not.

**The same fault, as an assertion.** `just demo-incident-verify` runs it with no
GUI and an exit code, and CI runs it on every PR — so the story cannot quietly
stop being true. It checks four things, and the fourth is the one that makes the
others mean anything:

1. an incident exists for the guest;
2. it names the hypervisor as its cause;
3. the cause is not itself filed as a symptom;
4. and **before** the fault, nothing is a symptom of anything — without which a
   correlator that attributed everything to everything would pass 1–3.
