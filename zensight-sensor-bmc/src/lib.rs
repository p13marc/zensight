//! Out-of-band hardware health, read from the BMC (#953, epic #952).
//!
//! # What was missing
//!
//! Nothing in ZenSight read a power supply. `sysinfo`'s `collect.power` is
//! RAPL energy, hwmon fan RPM and battery capacity — a CPU-and-laptop
//! surface. A grep for `ipmi`, `redfish` or `power supply` matched **zero**
//! files in the tree and zero of the 567 tracker issues.
//!
//! That is SYS-SUP-001 (power-supply state) entirely, and it is also the
//! blind spot behind SYS-SUP-010: on a server whose sensors sit behind a BMC
//! and are never exported to hwmon — most rack hardware — the platform
//! reported nothing about temperature or fans either, so "temperature and fan
//! speed" was met only on hosts that happened to have hwmon.
//!
//! # What this sensor deliberately is not
//!
//! **It cannot act.** No `Actions/Chassis.Reset`, no IPMI `chassis power`, no
//! write procedure of any kind — and `tests/registry_conformance.rs` fails the
//! build if one appears in the slice. A monitor that can power-cycle a server
//! is a different threat model from one that reads its fan speed, and crossing
//! that line is a decision to make deliberately, in its own issue, with the
//! #283 gate pattern. #956 is that decision being taken for PDU outlets; this
//! is not it.
//!
//! **It invents no thresholds.** Every assertion comes from the BMC's own
//! `Health` / `State` / `Redundancy` enums. The BMC knows the rating of the
//! hardware it is soldered to; we do not, and a number we made up about
//! someone else's silicon is worse information than none. Numeric thresholds
//! arrive with #931's `ThresholdsConfig`, from an operator who decided. The
//! BMC's own thresholds are published *beside* each reading, so a consumer can
//! make the comparison the hardware vendor intended.
//!
//! **"Not measured" is never a zero.** A BMC that does not answer yields a
//! `bmc-unreachable` assertion, a `reachable` gauge of 0, and **no** other
//! gauges — not a chassis of zeroes. A supply bay reported `Absent` publishes
//! `present: false` and no watts.
//!
//! # Attribution (#883)
//!
//! Everything is published under the **reporting host's** origin, with the
//! chassis in the key path and in the labels. A managed chassis is a facet of
//! the vantage point that polls it, exactly as a guest is a facet of its
//! hypervisor and a probe target of its prober: it does not publish for
//! itself, and filing its series under its own name would put them on no
//! host's card at all.

pub mod alerts;
pub mod config;
pub mod ipmi;
pub mod poller;
pub mod redfish;
mod telemetry_guard;
