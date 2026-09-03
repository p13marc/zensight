# zensight-sensor-bmc

Out-of-band hardware health, read from the machine's baseboard management
controller over Redfish (#953, epic #952).

## What was missing

| Question | Before |
|---|---|
| Is a power supply failed? | **Nothing in the tree read one.** A grep for `ipmi`, `redfish` or `power supply` matched zero files and zero of 567 tracker issues. |
| Has power redundancy been lost? | Same. |
| What is the inlet temperature? | Only where the board exports it to hwmon — which most rack hardware does not. |
| Is a fan stopped? | Same. |
| Is the chassis open? | Nothing. |

`sysinfo`'s `collect.power` is RAPL energy, hwmon fan RPM and battery
capacity: a CPU-and-laptop surface. On a server whose sensors sit behind a BMC
and never reach hwmon, ZenSight reported nothing about power, temperature or
fans at all. That is SYS-SUP-001 entirely, and the blind spot behind -010.

## There is no action surface

No `Actions/Chassis.Reset`, no IPMI `chassis power`, no write procedure. Two
tests enforce it: one parses the registry slice and fails on any
`kind = "write"`, the other greps the sensor's own source for `Actions/`,
`.post(` and `.patch(`. A monitor that can power-cycle a server is a different
threat model from one that reads its fan speed, and crossing that line is a
decision to make in its own issue with the #283 gate pattern.

## What it publishes

Everything under the **reporting host's** origin, with the chassis in the key
path and in the labels (#883) — a managed chassis is a facet of the vantage
point that polls it, not a machine that publishes for itself.

| Key | Meaning |
|---|---|
| `telemetry/bmc/{chassis}/reachable` | 1 or 0, every interval — an unreachable BMC reads as 0, never as silence |
| `telemetry/bmc/{chassis}/psu/{id}/{input,output,capacity}_watts` | absent on an empty bay and on a BMC that does not meter |
| `telemetry/bmc/{chassis}/psu/{id}/present` | what makes an absent wattage legible |
| `telemetry/bmc/{chassis}/fan/{id}/rpm` | absent on a BMC that reports only a percentage of maximum |
| `telemetry/bmc/{chassis}/thermal/{id}/celsius` | with `upper_critical_c` and `upper_warning_c` beside it |
| `state/bmc/chassis/{chassis}` | power state, intrusion, health, firmware, identity — and which Redfish surface answered |
| `state/bmc/chassis/{chassis}/{psu,fan,thermal}/{id}` | the full component documents |
| `state/bmc/evidence/device/{chassis}` | the BMC's view of the machine it manages, for the catalog |
| `state/bmc/alert/{key}` | the assertions below |

## Three rules the whole crate is arranged around

- **"Not measured" is never a zero.** A bay the BMC reports `Absent`
  publishes `present: false` and **no watts** — even when the firmware leaves
  a stale `0.0` in the document, which some does. A `0 W` reads as a supply
  drawing nothing, which is a different and wrong statement.
- **Every verdict is the BMC's.** `psu-failed`, `fan-failed`,
  `thermal-critical`, `psu-redundancy-lost` and `chassis-health` all read
  Redfish `Health` / `State` / `Redundancy`. The BMC knows the rating of the
  hardware it is soldered to; we do not. Its own thresholds are published
  beside each reading so a consumer can make the comparison the vendor
  intended. Numeric thresholds of your own arrive with #931.
- **`Unknown` is not a fault.** A BMC that answers without a health field has
  told us nothing, and a sensor that reads nothing as "broken" pages on
  missing data.

While a BMC is unreachable the component rules **keep their previous state**
rather than resolving: announcing that a failed supply is fine because we
cannot see it is worse than saying nothing.

## The Redfish surface is discovered, not assumed

Redfish 2020.4 deprecated `Chassis/{id}/Power` and `Thermal` for
`PowerSubsystem` and `ThermalSubsystem`, and a great deal of shipped firmware
serves only the old pair. The client tries the new one, falls back, and
**records which answered** in the chassis document — because a reading absent
on one is a different fact from the same reading absent on the other.

## Running it

```bash
cargo run -p zensight-sensor-bmc --release -- --config configs/bmc.json5
```

TLS verification is on by default. A BMC ships a self-signed certificate, so
there are two escape hatches and they are not equivalent: `ca_file` (the right
one — verification stays on) and `insecure` (the honest-but-loud one, warned at
every boot). Setting both is refused at startup.

Not part of `just run`: there is no BMC on a dev box, the same reason `pve` is
not.

## Docs

| File | What |
|---|---|
| [`docs/assertions.md`](docs/assertions.md) | every rule, what fires it, and what deliberately does not |
| [`docs/configuration.md`](docs/configuration.md) | every field, the startup refusals, and the TLS decision |
| [`src/lib.rs`](src/lib.rs) | scope and non-goals |
