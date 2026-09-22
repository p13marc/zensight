//! ZenSight parallax sensor: live video onto the Zenoh `@media` plane.
//!
//! Advertises local V4L2 cameras, remote RTSP cameras, and synthetic test
//! patterns as a stream catalogue (`@rpc/parallax/streams`), opens/closes encode
//! pipelines on `@rpc/parallax/stream/set` (`StreamControl`), and publishes opaque
//! encoded frames — H.264 access units and JPEG previews — on the
//! origin-scoped `@media/parallax/<stream>/…` keys with a CBOR `FrameMeta` attachment
//! per frame. Built on the `parallax` pipeline engine.
//!
//! Key layout (all origin-scoped under `zensight/v1/<origin>`):
//! - catalogue: `@rpc/parallax/streams` → `Vec<StreamDescriptor>`
//! - control:   `@rpc/parallax/stream/set` ← `Command<StreamControl>`
//! - status:    `state/parallax/stream/<stream>` → `StreamStatus`
//! - media:     `@media/parallax/<stream>/video/<codec>/<tier>` + `@media/parallax/<stream>/preview/jpeg`
//! - stats:     `telemetry/parallax/<stream>/stats/<metric>` (ordinary telemetry)

pub mod alerts;
pub mod annexb;
pub mod catalog;
pub mod command;
pub mod config;
pub mod discovery;
pub mod egress;
pub mod hotplug;
pub mod pipeline;
pub mod query;
pub mod reports;
pub mod session;
pub mod stats;

#[cfg(test)]
mod typed_subjects {
    use zensight_common::registry::parallax::Subject;
    use zensight_common::subject::TelemetrySubject;

    /// The generated subjects render the tails this sensor published by hand
    /// (#1274): byte-identical keys, so every consumer's series carries over.
    #[test]
    fn the_registered_families_render_their_tails() {
        for (subject, tail) in [
            (Subject::StreamsAdvertised, "streams/advertised"),
            (Subject::stats_fps("cam1"), "cam1/stats/fps"),
            (
                Subject::stats_encode_p95_ms("cam1"),
                "cam1/stats/encode_p95_ms",
            ),
            (
                Subject::rx_consumers("cam1", "low"),
                "cam1/rx/low/consumers",
            ),
            (
                Subject::rx_frame_age_ms_p50("cam1", "high"),
                "cam1/rx/high/frame_age_ms_p50",
            ),
        ] {
            assert_eq!(subject.tail(), tail);
        }
    }
}
