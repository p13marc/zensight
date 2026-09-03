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

A drive backplane, a riser, a battery, a CMOS fault — the BMC has verdicts on
things this sensor does not enumerate. Without the rollup rule, a BMC saying
"this machine is Critical" for a reason we do not model would be silently
dropped, which is exactly the blind spot the sensor exists to close. Its
message points at the BMC's own event log, because that is where the detail is.

## Reconciliation

Every rule reconciles every sweep, so a cleared condition resolves rather than
firing until restart. Reconciliation is **scoped to the chassis**
(`reconcile_labeled(rule, "chassis", …)`): one process polls several endpoints,
and one chassis's recovery must not resolve another's fault.

`ALL_RULES` is also what the reporter adopts on restart (#882), so a rule this
build can no longer raise retires its inherited alerts instead of leaving them
firing forever.

## Attribution

Every alert's `source` is the **reporting host**, with the chassis as a label
(#883). Filing an alert under the chassis would put it on no host's card in the
GUI, which groups by `(protocol, source)`. An e2e test asserts it.
