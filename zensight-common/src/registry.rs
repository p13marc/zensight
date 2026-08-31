//! The compiled subject registry (RFC 08): per-producer `Subject`/`ProcedureId`
//! enums, `AnySubject` dispatch, `REGISTRIES`, `registry_toml()`, and
//! `is_registered_telemetry()`.
//!
//! Generated at build time by `zenkey-build` from the registry TOMLs in
//! `zensight-common/registry/*.toml` — edit those files (and the append-only
//! `deprecated.lock` ledger), never this module's output.

// zenkey-build renders a service with no `common`-mapped subjects as a
// `match s { _ => None }` arm (the `@desired` slice is the first such), which
// clippy flags inside the GENERATED code. Module-level allow, because the
// fix belongs upstream in the generator, not in a file nobody edits.
#![allow(clippy::match_single_binding)]

include!(concat!(env!("OUT_DIR"), "/zenkey_registry.rs"));
