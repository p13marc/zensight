# Assertions

Every rule reads a verdict the **BMC** reached. None of them takes a number
from this sensor, and that is the design rather than a limitation: the BMC
knows the rating of the hardware it is soldered to and we do not, so a
threshold invented here would be a guess about someone else's silicon — worse
information than none.

`alerts::grade` is pure — no bus, no HTTP — so the whole table is testable
against documents.

| Rule | Fires when | Severity |
|---|---|---|
| `bmc-unreachable` | the BMC has not answered for `unreachable_cycles` consecutive sweeps (default 3) | critical |
| `psu-failed` | a **present** supply's Redfish `Health` is Warning or Critical | the BMC's own — Warning maps to warning, Critical to critical |
| `psu-redundancy-lost` | the supply's redundancy group reports degraded or failed | warning / critical |
| `psu-absent` | a bay that **was** populated now reads `Absent` | warning |
| `fan-failed` | a present fan's `Health` is Warning or Critical | critical |
| `thermal-critical` | the sensor's `Health` is faulted **or** its reading is at/above the BMC's own `UpperThresholdCritical` | critical (warning for a Warning health under threshold) |
| `chassis-health` | the chassis rollup is faulted | the BMC's own |
| `drive-failed` | a **present** drive's `Health` is faulted, **or** its `FailurePredicted` bit is set (#1140) | the BMC's own; a prediction on an otherwise-OK drive is a warning, because the drive is still serving |
| `memory-failed` | a **present** DIMM's `Health` is faulted (#1140) | the BMC's own |
| `redundancy-lost` | a `PowerSubsystem`/`ThermalSubsystem` **group** reports degraded or failed (#1140) | warning / critical |

## What deliberately does not fire

**`Unknown` health.** That is the BMC declining to say. Treating it as a fault
is paging on missing data — the failure the whole crate is arranged against.

**An empty bay that was always empty.** `psu-absent` is off by default *and*
requires having seen the bay populated earlier in this process's life. A
chassis shipped with one supply in a two-bay backplane is normal and
permanent; firing on it means every such machine arrives with a standing alert
nobody can clear.

**A temperature with no threshold to compare it against.** 200 °C with no
`UpperThresholdCritical` and no health verdict asserts nothing. It looks alarming
and it is not a verdict this sensor is entitled to reach.

**Anything at all, while the BMC is unreachable.** A BMC that did not answer
produced no components; grading them would resolve every one of them as
"recovered" — announcing that a failed power supply is fine because we cannot
see it. `bmc-unreachable` fires and the component rules keep their previous
state until the BMC answers again. (The lesson SNMP's `device_answered` guard
already paid for.)

## Why `chassis-health` exists

A riser, a battery, a CMOS fault — the BMC has verdicts on things this sensor
does not enumerate. Without the rollup rule, a BMC saying "this machine is
Critical" for a reason we do not model would be silently dropped, which is
exactly the blind spot the sensor exists to close. Its message points at the
BMC's own event log, because that is where the detail is.

**Two of the things it used to gesture at are now named** (#1140). A drive or a
DIMM the BMC had already marked rolled up into `Chassis.Status.Health` and
nowhere else, so the operator was told to go and read an event log about a fact
this sensor could have put in the alert. `drive-failed` and `memory-failed`
read `Systems/{id}/Storage/{ctrl}/Drives` and `Systems/{id}/Memory` — scoped to
the systems **this chassis links**, exactly as the identity claim is and for
the same reason (#1110): one Redfish service fronts several machines on a blade
enclosure, and walking `/redfish/v1/Systems` wholesale would put every node's
DIMMs on every chassis. `chassis-health` stays, because the set of things a BMC
has an opinion about is not ours to close.

## Why `redundancy-lost` is not `psu-redundancy-lost`

`psu-redundancy-lost` reads the `Redundancy` array on a **member** — a supply's
copy of its own group's status. `redundancy-lost` reads the group, off
`PowerSubsystem.Redundancy` and `ThermalSubsystem.Redundancy` (#1140).

They are not the same reading, and the difference is the failure. A supply that
is itself healthy commonly carries no `Redundancy` array at all — the fixture
in `tests/e2e.rs` is a real shape, and its surviving supply says nothing about
the group. So on a chassis whose failed supply has been *pulled*, the
per-member rule has no input left and resolves, announcing that redundancy is
fine at the moment there is none. The group knows: `MinNumNeeded: 2` with one
member in the set.

Both rules stay. The member's copy is the earlier signal on firmware that fills
it in, and the group is the one that survives the member going away.

## Reconciliation

Every rule reconciles every sweep, so a cleared condition resolves rather than
firing until restart. Reconciliation is **scoped to the chassis**
(`reconcile_labeled(rule, "chassis", …)`): one process polls several endpoints,
and one chassis's recovery must not resolve another's fault.

That sentence was true of the intent and false of the code until #1130. The
label was the **endpoint's** name, and one Redfish service fronts several
chassis on a blade enclosure or a four-node Twin — so a `still` list computed
from `sweeps.first()` reconciled the whole endpoint, and a failed supply in
chassis 2 was not merely missed but actively **resolved**, every sweep. The
label is the key chunk now, `{endpoint}-{chassis id}`, and each chassis is
graded and reconciled in its own namespace.

Two rules sit outside that scoping, on purpose:

- **`bmc-unreachable` is the endpoint's.** A BMC that did not answer returned
  no chassis list, so there is nothing else to name it with, and inventing a
  chassis chunk would claim a chassis this sensor has never seen. It is the
  one rule reconciled under the endpoint's own chunk, and the only one.
- **A chassis that stops being listed** has its rules reconciled to empty, so
  a pulled blade does not fire forever. A chassis that is still *listed* but
  whose sweep failed does **not** — "we could not read it" is not "it
  recovered", the same rule as the unreachable-BMC hold above.

`ALL_RULES` is also what the reporter adopts on restart (#882), so a rule this
build can no longer raise retires its inherited alerts instead of leaving them
firing forever.

## Attribution

Every alert's `source` is the **reporting host**, with the chassis as a label
(#883). Filing an alert under the chassis would put it on no host's card in the
GUI, which groups by `(protocol, source)`. An e2e test asserts it.

The `chassis` label is the **key chunk**, `{endpoint}-{Redfish chassis id}`,
and not the endpoint's name. `alert_key` hashes the discriminating labels, so
with the endpoint there two chassis of one service that both have a bay `0` —
the normal case, since every chassis numbers its bays from zero — produced the
**same** alert key and took turns overwriting each other (#1130).

The summary names the same place in prose: `rack-a-1 chassis 2: PSU 1 health
is Critical`. The chunk is for machines, the sentence is for the person reading
the page.
