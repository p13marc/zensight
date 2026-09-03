# Detection latency

*What "under 10 seconds" means per sensor, and which shipped defaults meet it.*

SYS-SUP-004 asks that a newly connected communication means be detected and its
state shown in **under 10 seconds**. The bound is reachable, but it is not a
property of ZenSight as a whole: it is a property of **each sensor's cadence**,
and for pollers it is a **configuration** question rather than a code one. This
page states which defaults meet it, which do not, and what to change.

Nothing here is an aspiration. The two figures in the "measured" column come
from [`zensight-sensor-netlink/tests/detection_latency.rs`](../zensight-sensor-netlink/tests/detection_latency.rs)
and [`zensight-sensor-sysinfo/tests/detection_latency.rs`](../zensight-sensor-sysinfo/tests/detection_latency.rs),
which measure what a **subscriber receives** — not what a sensor believes it
published — and print the number so a regression from 400 ms to 8 s is visible
in the log rather than hidden behind a pass.

## Worst-case detection latency by sensor

For an event-driven sensor, detection is the kernel notification plus delivery.
For a poller it is **one poll interval** plus delivery: a change occurring just
after a poll is invisible until the next one, so the interval *is* the bound.

| Sensor | Mechanism | Shipped default | Meets 10 s? | Measured |
|---|---|---|---|---|
| `netlink` | RTNETLINK events (`poll_interval_secs: 2` backs them up) | event-driven | **yes** | start → subscriber **5.8 ms** |
| `sysinfo` | poll | `poll_interval_secs: 5` | **yes** | sample-to-sample **4.5 s** |
| `modbus` | poll | `poll_interval_secs: 5`–`10` | **yes** | — |
| `container` | poll | `poll_interval_secs: 30` | **no** | — |
| `systemd` | D-Bus signals + poll | `poll_interval_secs: 15` | signals yes, poll no | — |
| `snmp` | poll | `poll_interval_secs: 30`–`60` | **no** | — |
| `probe` | poll | `interval_secs: 60` (floor 5) | **no** at default | — |
| `pve` | poll | `poll_interval_secs: 60` | **no** | — |
| `hostspec` | poll | `interval_secs: 60` | **no** | — |
| Zenoh delivery + GUI apply | — | — | sub-second | included above |

**So the honest answer to SYS-SUP-004 is: event-driven sensors yes; pollers only
if configured for it.** That sentence was true before this page existed and was
written down nowhere.

## Making a poller meet the bound

Set the interval to **≤ 5 s** — half the budget, leaving room for delivery and a
missed tick:

```json5
{ sysinfo: { poll_interval_secs: 5 } }   // the default already
{ probe:   { interval_secs: 5 } }        // the configured floor
```

**`snmp` should not be configured this way and the bound should not be claimed
for it.** A 5 s walk against every configured device is a load an SNMP agent on
a switch will not thank you for, and it does not scale past a handful of
devices. The SNMP-side answer to link-state detection is **traps**, which are
event-driven and which the trap receiver already handles — configure the device
to send link up/down traps rather than polling faster.

`pve` and `container` are similar: the thing they observe (a guest's config, a
container's lifecycle) does not change on a ten-second horizon, and polling them
at 5 s spends API calls to detect nothing.

## What "detection" means here, and what it does not

Detection in ZenSight is **generic and IP-level**:

- `netring`'s passive asset inventory (MAC/IP, vendor, role from traffic),
- `netlink`'s neighbour table (ARP/NDP) and link state,
- `snmp`'s opt-in, propose-only subnet discovery (#541) — it never auto-adds,
- the correlator's entity fusion, which turns those observations into one host.

There is **no protocol knowledge of RF, satellite or acoustic links**. A device
on such a link is an SNMP or probe target like any other, and is detected as
whatever it presents on the IP network. If SYS-SUP-004 means *typed
classification* of a communication means — "this is a satellite modem, and it is
up" — that is a separate piece of work and it needs the device list first.
Reading the requirement as "the link's state is observable within 10 s" is what
this page and its tests answer.

## The privileged test leg

`a_new_interface_is_detected_fast` measures the literal requirement — an
interface appears, how long until a subscriber has it — and needs
`CAP_NET_ADMIN` to create a dummy interface. It **skips with a printed reason**
when unprivileged rather than passing silently, because a capability test that
quietly passes without the capability reports a bound nobody measured. Run it on
a privileged CI leg or locally with `CAP_NET_ADMIN`:

```bash
sudo -E cargo test -p zensight-sensor-netlink --test detection_latency -- --nocapture
```
