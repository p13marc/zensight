//! The compiled subject registry (RFC 08): per-producer `Subject`/`ProcedureId`
//! enums, `AnySubject` dispatch, `REGISTRIES`, `registry_toml()`, and
//! `is_registered_telemetry()`.
//!
//! Generated at build time by `zenkey-build` from the registry TOMLs in
//! `zensight-common/registry/*.toml` — edit those files (and the append-only
//! `deprecated.lock` ledger), never this module's output.

include!(concat!(env!("OUT_DIR"), "/zenkey_registry.rs"));
