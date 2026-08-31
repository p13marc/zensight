//! Proxmox VE sensor (#818) — the hypervisor as a hypervisor.
//!
//! ZenSight measured the reference fleet's hypervisor as a Linux box: three
//! native binaries reporting CPU, memory, disks, units and the journal.
//! Everything that made it a *hypervisor* was invisible, and the 2026-08-28
//! audit found three things by hand that this sensor asserts continuously:
//!
//! | Found by hand, weeks late | Asserted here |
//! |---|---|
//! | VM 140 had `onboot=0` and would not have survived a host reboot | `guest-onboot-off` |
//! | VM 140's NIC had no `firewall=1`, so its firewall file was inert and :8000 was open to the whole service zone | `guest-nic-firewall-off` |
//! | 990 GB provisioned on a 937 GB pool | `pool-overcommitted` |
//!
//! None of those is a metric that spikes. They are **configuration facts that
//! stopped matching intent**, which is what a polling sensor with per-device
//! liveness is for. The gauges exist so the guests get device cards,
//! Prometheus gets series and the family-coverage audit has families; the
//! real output is the state documents and the alert set.
//!
//! # What this sensor deliberately is not
//!
//! **It has no action surface at all.** Not disabled, not gated — absent. A
//! monitor that can stop a VM is a different threat model, and if one is ever
//! added it must be default-off with an allowlist, the way the systemd
//! sensor's `actions` block already is, as a separate and deliberate
//! decision. Nothing in this crate constructs a non-GET request.
//!
//! It is also not a replacement for the guests' own sensors: it knows what
//! the hypervisor knows. What it adds to the catalog is the *join* — a
//! third-party identity claim per guest, carrying the name and the configured
//! MACs, so the hypervisor's view of a VM fuses with that VM's own reports.

pub mod alerts;
pub mod api;
pub mod config;
pub mod poller;
mod telemetry_guard;
