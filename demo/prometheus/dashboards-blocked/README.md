# Dashboards that still cannot be written, and why

**Nothing in this directory is provisioned.** It is a *sibling* of
`../dashboards/`, not a child, because a Grafana file provider walks
subdirectories — a `blocked/` folder inside the mounted path would be
provisioned as a Grafana folder full of empty panels, which is the outcome this
directory exists to prevent.

## What got unblocked

The registry-driven naming redesign (**#764**) landed, so a per-entity subject
is now a **label** rather than part of the metric name:

| Sensor key | Before | After |
|---|---|---|
| netlink `iface/{iface}/rx_bytes` | `zensight_netlink_iface_eth0_rx_bytes` | `zensight_netlink_iface_rx_bytes_total{iface="eth0"}` |
| sysinfo `disk/{mount}/inodes_total` | `zensight_sysinfo_disk_root_inodes_total` | `zensight_sysinfo_disk_inodes_total{mount="root"}` |

**`ZenSight — Network (netlink)`** has moved into `../dashboards/` and is
provisioned. Every one of its nine panels was checked against a real netlink
scrape, not derived on paper.

## What is still blocked

### netring — written, not verified

The naming works the same way (`flow/by_l4/{proto}/bytes_total` →
`zensight_netring_flow_by_l4_bytes_total{proto="tcp"}`), so these panels are
expected to work. They are parked only because netring needs `CAP_NET_RAW` to
capture, and the panels have not been run against a live scrape. Every provisioned
dashboard in this repo has been; these have not, and shipping an unverified panel
is how a folder of empty graphs starts.

To unpark: `just netring` (which runs `just caps`), scrape, confirm, move the file.

### SNMP per-interface — unblocked, but not verified here

`sum by (index)` **is now writable**. Registry `snmp` **1.8** (#779) registers
the `ifTable`/`ifXTable` columns explicitly — one subject per column,
`{device}/if/{index}/<column>` (plus the `.rate` sibling the poller derives for
every counter) — so #764's family rule names from the literal chunks and every
variable becomes a label:

| | series |
|---|---|
| before | `zensight_snmp_if_1_in_octets_total` and `zensight_snmp_if_2_in_octets_total`, two unrelated families |
| after | `zensight_snmp_if_in_octets_total{index="1"}` / `{index="2"}`, one family |

It is registered **column by column** rather than as
`{device}/if/{index}/{column}`, because a `{column}` variable would be dropped
from the family name along with the others — collapsing counters, gauges and
strings into one `zensight_snmp_if` family and emitting two `# TYPE` lines for
one name, which is the scrape-killer class #752 fixed.

**No SNMP dashboard is shipped yet, and the reason is this directory's whole
rule**: a panel is provisioned only after it has been checked against a real
scrape, and that needs a real SNMP device (or the simulated agent from
`zensight-sensor-snmp/tests/e2e.rs`) polled by a running exporter. Nobody has
done that yet. The naming half is done; the verification half is not.

`cpu/{index}/…`, `ip/{index}/…` and `storage/{index}/…` still ride the catch-all
and still bake their index into the name — the same change again, not yet made.

Richer still, `zensight-common/src/interfaces.rs` already publishes an
`InterfaceTable` per device whose own doc-comment says it exists to replace
"every consumer's stringly-typed reassembly of `if/<index>/<column>` metric
names" — so `ifname` and `ifalias` are the natural next dimension now that the
index is a real one.
