//! zensight-sensor-hostspec (#821): machine-checked desired-state assertions.
//!
//! The reference deployment keeps a careful document called "desired state
//! per machine" — package deltas, mounts, lingering guards — and nothing
//! checks it; every finding of its audits has the shape *reality drifted
//! from the document and nobody noticed for weeks*. This sensor is that
//! document as a machine-checked contract: a **closed vocabulary** of
//! read-only assertions (mounts, files, listening sockets, symlinks,
//! absence, file content, permissions), evaluated on a tick, published as
//! alerts with the failing assertion in the labels, hot-swappable over
//! `@rpc/hostspec/expectations/set`, and answerable — "what is this host
//! being held to" — on `@rpc/hostspec/spec`.
//!
//! **Deliberately absent, forever:** any command/run assertion — that is a
//! remote-execution surface wearing a monitoring hat — and (a #821 user
//! call) any binary/version assertion: this sensor **executes nothing**,
//! reads only `/proc`, `/etc/passwd`+`group` and `lstat`/`readlink`/bounded
//! file reads, and needs no capabilities of any kind. It is the least
//! privileged sensor in the fleet.
//!
//! **The honest Ansible overlap:** this looks like configuration
//! management's job and is not — Ansible converges the machine at run time;
//! hostspec notices drift *between* converges, and it is the thing that
//! tells you the converge never ran. If an IaC track lands, generate the
//! expectation set from the same inventory rather than authoring it twice.
//!
//! **Scope honesty:** listeners are read from this network namespace
//! (`/proc/net/tcp{,6}`) — a containerized service's socket lives in its
//! own namespace and is invisible here. Owner/group names resolve through
//! `/etc/passwd`/`/etc/group` only; NSS/LDAP hosts should assert numeric
//! ids.

pub mod command;
pub mod config;
pub mod map;
pub mod observe;
pub mod sentinel;
mod telemetry_guard;
