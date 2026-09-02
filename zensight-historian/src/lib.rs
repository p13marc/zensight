//! `zensight-historian` — the fleet's telemetry history, as a service.
//!
//! # Why this exists
//!
//! Telemetry was the only wire class with no history path for a second
//! process. Logs have `@rpc/logs/events` over a durable redb store; events
//! have a router `fs` storage plus a startup GET; state has seed storages.
//! Telemetry had two things, and neither is a history: the AdvancedPublisher's
//! ten-sample-per-key cache, and a redb file inside the Iced binary that only
//! the GUI which wrote it could read. On the reference fleet that GUI is open
//! for minutes a week, so what it holds is mostly gaps.
//!
//! This is an ordinary Zenoh application that closes that: it subscribes
//! `v1/*/telemetry/**` with history and recovery, writes the same tiers the
//! GUI's cache does — the shared [`zensight_store`] crate — and serves typed,
//! bounded, cursor-paginated reads over them. The pattern is the logs sensor's,
//! applied to the class that never had it.
//!
//! # What it is not
//!
//! **Not a router plugin, and not a second database process.** RFC 04 §4's
//! InfluxDB `timeseries` storage was the obvious alternative and does not fit:
//! `zenoh-backend-influxdb` v2 cannot answer `*`/`**` selectors, a `_time=` GET
//! has no aggregation or downsampling so a day of per-second samples travels
//! raw, and an out-of-tree plugin cannot run in CI — where `demo-smoke` and
//! `conformance` execute workspace binaries. Prometheus remote-write already
//! ships for deployments that want a real TSDB.
//!
//! **Not a service origin.** It is a host-origin *producer*
//! (`v1/h-…/@rpc/historian/range`). Service origins exist for single-writer
//! fleet state — `@catalog`, `@desired` — and this writes none; it only answers
//! RPC. Two historians, one per site, are then ordinary RFC 05 §2.1 fan-in with
//! no claim protocol to get wrong.
//!
//! **Not a publisher of telemetry.** RFC 04 §1.1: a history service that
//! re-emitted what it ingested would be a loop with a database in it. Its own
//! numbers ride the health document and `@rpc/historian/stats`.
//!
//! **Not a query language.** Structured, typed, bounded range queries only.
//!
//! # Series identity
//!
//! `(origin, producer, subject)` — the wire key minus the class chunk. It is
//! derivable from a sample alone, so it survives a catalog merge and a
//! correlator outage, and it is the same name the GUI's local cache uses, which
//! is what lets a chart read the fleet's history when a historian is alive and
//! fall back to the local one when none is. Entity resolution is a query-time
//! join, not a storage key.

pub mod config;
pub mod ingest;
pub mod query;
pub mod timeline;

/// The producer chunk this service publishes and serves under.
pub const PRODUCER: &str = "historian";
