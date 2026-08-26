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
| Images | `prom/prometheus:v3.14.0`, `grafana/grafana:13.2.0` | `grafana/otel-lgtm:0.11.14` |

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

If the target is `up` and `/metrics` has content, the panel is probably one whose
metric name is not aggregatable yet. See
[`prometheus/dashboards-blocked/README.md`](prometheus/dashboards-blocked/README.md)
— netlink, netring and SNMP panels are deliberately **not** provisioned because
their per-entity subjects are still baked into metric *names*.

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
    dashboards/                     import through the Grafana UI
```

`dashboards-blocked/` is a **sibling** of `dashboards/`, not a child, because a
Grafana file provider walks subdirectories — a `blocked/` folder inside the
mounted path would be provisioned as a folder full of empty panels.
