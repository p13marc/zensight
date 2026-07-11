//! ZenSight parallax sensor: live video onto the Zenoh `@media` plane.
//!
//! Advertises local V4L2 cameras, remote RTSP cameras, and synthetic test
//! patterns as a stream catalogue (`@/query/streams`), opens/closes encode
//! pipelines on `@/commands/stream` (`StreamControl`), and publishes opaque
//! encoded frames — H.264 access units and JPEG previews — on the
//! host-scoped `@media/<stream>/…` keys with a CBOR `FrameMeta` attachment
//! per frame. Built on the `parallax` pipeline engine.
//!
//! Key layout (all host-scoped under `zensight/parallax/<source>`):
//! - catalogue: `@/query/streams` → `Vec<StreamDescriptor>`
//! - control:   `@/commands/stream` ← `Command<StreamControl>`
//! - status:    `@/status/streams` → `StreamStatus`
//! - media:     `@media/<stream>/video/h264/<profile>` + `@media/<stream>/preview/jpeg`
//! - stats:     `<stream>/stats/<metric>` (ordinary telemetry)

pub mod catalog;
pub mod command;
pub mod config;
pub mod egress;
pub mod pipeline;
pub mod query;
pub mod session;
