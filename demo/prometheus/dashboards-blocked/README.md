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

`sum by (index)` **is now writable, for all five indexed tables.** Registry
`snmp` **1.8** (#779) registered the `ifTable`/`ifXTable` columns explicitly —
one subject per column, `{device}/if/{index}/<column>`, plus the `.rate` sibling
the poller derives for every counter — and **1.9** (#783) did the same for
`hrProcessorTable`, `ipAddrTable` and `hrStorageTable`. #764's family rule names
from the literal chunks, so every variable becomes a label:

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

The other three landed in registry `snmp` **1.9** (#783), on the same terms:
`zensight_snmp_cpu_load{index="1"}`, `zensight_snmp_ip_if_index{index="1"}`,
`zensight_snmp_storage_size{index="1"}`.

Two differences from ifTable worth knowing before writing a panel:

- **No `.rate` siblings**, and that is correct rather than missing. The poller
  derives a rate from the wire tag (`Counter32`/`Counter64`), and not one column
  of those three tables is a counter — hrStorage is INTEGER throughout,
  hrProcessorLoad is INTEGER, ipAddrTable is IpAddress/INTEGER.
- **The `ip/` group's scalars** (`ip_forwarding`, `ip_default_ttl`,
  `ip_in_receives_total` and its rate) are 3-chunk keys and stay on the
  catch-all. Their names already carry no index, so nothing needed fixing, and
  they carry **no `index` label** — do not write `sum by (index)` over them.

What is still missing for every SNMP panel is the same thing: a real device
polled by a running exporter. The naming half is done for all five tables; the
verification half is not.

Richer still, `zensight-common/src/interfaces.rs` already publishes an
`InterfaceTable` per device whose own doc-comment says it exists to replace
"every consumer's stringly-typed reassembly of `if/<index>/<column>` metric
names" — so `ifname` and `ifalias` are the natural next dimension now that the
index is a real one.
