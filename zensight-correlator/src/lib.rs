//! ZenSight identity correlator — the fleet entity service.
//!
//! A small headless service (deployed like an exporter, one per fleet) that is
//! the **single writer** of the derived entity keyspace. It subscribes only to
//! the evidence keyspace (`zensight/_meta/evidence/**`, never the telemetry
//! firehose), merges [`HostEvidence`](zensight_common::HostEvidence) claims into
//! [`HostEntity`](zensight_common::HostEntity) docs via a deterministic
//! union-find over ranked identity rules, and publishes the materialized entity
//! view (cached, re-emitted, tombstoned) plus late-joiner queryables.
//!
//! ```text
//! evidence/**  ─┐
//!               ├─> [subscriber] ─mpsc─> [engine] ─(merge)─> [publisher] ─> entity/**
//! liveness   ──┘                              └── stores (evidence TTL, name map)
//! ```
//!
//! See `docs/CORRELATION-ANALYSIS.md` §9.3 and `docs/KEYSPACE.md`.

pub mod config;
pub mod engine;
pub mod guard;
pub mod merge;
pub mod subscriber;

pub use config::CorrelatorConfig;
pub use engine::{Engine, EvidenceMsg};
