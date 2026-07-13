//! ZenSight parallax sensor: live video onto the Zenoh `@media` plane.
//!
//! Advertises local V4L2 cameras, remote RTSP cameras, and synthetic test
//! patterns as a stream catalogue (`@rpc/parallax/streams`), opens/closes encode
//! pipelines on `@rpc/parallax/stream/set` (`StreamControl`), and publishes opaque
//! encoded frames — H.264 access units and JPEG previews — on the
//! origin-scoped `@media/parallax/<stream>/…` keys with a CBOR `FrameMeta` attachment
//! per frame. Built on the `parallax` pipeline engine.
//!
//! Key layout (all origin-scoped under `zensight/@v1/<origin>`):
//! - catalogue: `@rpc/parallax/streams` → `Vec<StreamDescriptor>`
//! - control:   `@rpc/parallax/stream/set` ← `Command<StreamControl>`
//! - status:    `state/parallax/stream/<stream>` → `StreamStatus`
//! - media:     `@media/parallax/<stream>/video/h264/<profile>` + `@media/parallax/<stream>/preview/jpeg`
//! - stats:     `telemetry/parallax/<stream>/stats/<metric>` (ordinary telemetry)

pub mod alerts;
pub mod annexb;
pub mod catalog;
pub mod command;
pub mod config;
pub mod egress;
pub mod pipeline;
pub mod query;
pub mod session;
pub mod stats;
