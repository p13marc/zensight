# Dashboards that do not work yet, and why

**Nothing in this directory is provisioned.** It is a *sibling* of
`../dashboards/`, not a child, because a Grafana file provider walks
subdirectories — a `blocked/` folder inside the mounted path would be
provisioned as a Grafana folder full of empty panels, which is precisely the
outcome this directory exists to prevent.

These panels are kept because the queries are correct *for the naming the
exporter is moving to*. Wiring them up today would show empty graphs and read as
"the exporter is broken", when the truth is narrower and fixable.

## The constraint

`build_metric_name` (`zensight-exporter-prometheus/src/mapping.rs`) consults
`zensight_common::semconv::metric_semconv`, which covers **only** `Protocol::Sysinfo`
and `Protocol::Systemd`. Everything else falls through to `sanitize_metric_name`,
which turns `/` into `_` — baking the per-entity subject into the **name**:

| Sensor key | Today's series | Aggregatable? |
|---|---|---|
| netlink `iface/eth0/rx_bytes` | `zensight_netlink_iface_eth0_rx_bytes` | no |
| netring `flow/by_l4/tcp/bytes_total` | `zensight_netring_flow_by_l4_tcp_bytes_total` | no |
| netring `bandwidth/https/bytes_per_sec` | `zensight_netring_bandwidth_https_bytes_per_sec` | no |
| snmp `sw1/if/1/in_octets` | `zensight_snmp_sw1_if_1_in_octets` | no |
| sysinfo `network/eth0/rx_bytes` | `zensight_system_network_io{device,direction}` | **yes** |
| systemd `unit/sshd.service/active` | `zensight_systemd_unit_active{unit}` | **yes** |

So `if/1/in_octets` and `if/2/in_octets` are two unrelated metric *families* and
`sum by (interface)` cannot be written. The only workaround available today is a
name regex — `{__name__=~"zensight_netlink_iface_.*_rx_bytes"}` — which returns
the right series but still cannot `sum by (iface)` and legends as the raw metric
name. Not worth shipping.

## What unblocks them

The registry-driven naming redesign (**epic #750**). The registry already
declares `path = "iface/{iface}/rx_bytes"`, and the generated `AnySubject`
already exposes `pattern()` and `vars()` generically — so the rename is
mechanical: strip `{...}` chunks out of the pattern for the name, and push
`vars()` into the label set.

| Panel | Query it wants | Blocked on |
|---|---|---|
| Top interfaces by rx | `topk(5, sum by (iface) (rate(zensight_netlink_iface_rx_bytes[$__rate_interval])))` | netlink rename (#764) |
| Interface errors / drops | `sum by (iface) (rate(zensight_netlink_iface_rx_errors[$__rate_interval]))` | netlink rename (#764) |
| Traffic by L4 protocol | `sum by (proto) (rate(zensight_netring_flow_by_l4_bytes_total[$__rate_interval]))` | netring rename (#764) |
| Bandwidth by application | `topk(10, zensight_netring_bandwidth_bytes_per_sec)` legend `{{app}}` | netring rename (#764) |
| Anomalies by kind | `sum by (kind) (rate(zensight_netring_anomaly_total[$__rate_interval]))` | netring rename (#764) |
| SNMP per-interface | `sum by (ifname) (rate(zensight_snmp_if_in_octets[$__rate_interval]))` | **not** the rename — see below |

## SNMP is a different problem, and the rename will not fix it

`zensight-common/registry/snmp.toml` registers the catch-all `{device}/{metric...}`.
The OID tail is defined by the polled device, so **no exporter-side naming rule
can factor the index out** — only `device` factors. The fix has to come from the
producer: attach `index` as a label at the sensor
(`zensight-sensor-snmp/src/poller.rs`, tracked as **#769**), where the MIB
resolver already knows the template. Richer still, `zensight-common/src/interfaces.rs`
already publishes an `InterfaceTable` per device whose own doc-comment says it
exists to replace "every consumer's stringly-typed reassembly of
`if/<index>/<column>` metric names".

Until one of those lands, a per-interface SNMP panel cannot be written honestly
at all.
