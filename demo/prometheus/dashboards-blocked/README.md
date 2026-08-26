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

### SNMP per-interface — needs a registry change

`sum by (ifname)` still cannot be written, and #764 does **not** fix it.

`zensight-common/registry/snmp.toml` registers the catch-all
`{device}/{metric...}`. The metric tail is defined by the polled device, so the
pattern has no literal chunks and the family rule has nothing to work from —
`if/1/in_octets` and `if/2/in_octets` remain two unrelated families.

**#769 got half of it**: `MibResolver::resolve_indexed` now hands back the table
index the resolver was already computing and discarding, and the poller attaches
it as an `index` **label**. So the index is queryable. It is not yet
aggregatable, because it is still in the name.

The remaining step is for `snmp.toml` to register `{device}/if/{index}/{column}`
ahead of the catch-all, at which point #764's generic rule takes over with no
special case at all. That is a registry change with its own compatibility story
(`compat = "backward"`, a version bump, the retirement ledger), which is why it
is filed separately rather than folded into #769.

Richer still, `zensight-common/src/interfaces.rs` already publishes an
`InterfaceTable` per device whose own doc-comment says it exists to replace
"every consumer's stringly-typed reassembly of `if/<index>/<column>` metric
names" — so `ifname` and `ifalias` are available once the index is a real
dimension.
