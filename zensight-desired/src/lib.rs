//! `zensight-desired` — the fleet policy compiler (#938, epic #902).
//!
//! # Why this exists
//!
//! `@desired` shipped in 0.12.0 as *"fleet configuration as desired state
//! instead of eighteen hand-edited JSON5 files across six machines"*. The
//! consumer shipped with it, the router storage shipped with it, the
//! never-list shipped with it — and **nothing in this repository published a
//! single desired document**. The author of the fleet's desired state was a
//! private script somewhere else. This crate is the author.
//!
//! It reads one `fleet-policy.json5`, asks the catalog what hosts exist and
//! what they are, and publishes the per-host documents that follow.
//!
//! # What it is not
//!
//! **Not a configuration-management system.** It does not run commands, copy
//! files, install packages or reach a host at all. It publishes documents to a
//! bus that hosts reconcile *themselves*, which is the difference RFC 12 draws
//! between convergence and durable pub/sub imperatives — and the reason a host
//! that was offline during a change picks it up when it returns rather than
//! missing it forever.
//!
//! **Not a second identity service.** It has no opinion about what a host is;
//! it asks `@catalog`, which is the only component that ran the union-find.
//!
//! **Not a template engine.** Classes compose by overlay, not by
//! interpolation. There is no expression language, and adding one is how a
//! policy file stops being reviewable.
//!
//! **Not able to carry a secret.** Every document is checked against the
//! `@desired` never-list before publication (`zensight_common::desired`), and
//! the payload types themselves have no field for credentials — SNMP
//! communities and probe headers stay in each host's own file config and are
//! referenced by name.
//!
//! # The property that makes it safe to run unattended
//!
//! **A restart with an unchanged policy and an unchanged fleet publishes
//! nothing.** Compilation is pure, output is canonical JSON, and publication
//! is gated on a content diff against what the storage already holds. Without
//! that, every restart would rewrite every document on every host, and the
//! `applied/<topic>` markers would show a fleet permanently reconverging.

pub mod compile;
pub mod config;
pub mod fleet;
pub mod merge;
pub mod overrides;
pub mod policy;
pub mod publish;
pub mod serve;

/// The service origin this daemon writes under (RFC 07 §3).
pub const ORIGIN: &str = "@desired";
