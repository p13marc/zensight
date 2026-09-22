//! Zenoh sensor for Syslog telemetry.
//!
//! This crate provides a syslog receiver that publishes messages to Zenoh.
//!
//! # Supported Formats
//!
//! - RFC 3164 (BSD syslog)
//! - RFC 5424 (structured syslog)
//!
//! # Key Expression Format
//!
//! Per-line messages feed the bounded `@rpc/logs/events` ring (#358, pull-only);
//! the derived rollups ride the telemetry bus under:
//! ```text
//! zensight/v1/<origin>/telemetry/logs/...
//! ```

pub mod built;
pub mod commands;
pub mod config;
pub mod dedup;
pub mod evidence;
pub mod file_source;
pub mod filter;
pub mod ingest;
#[cfg(feature = "journald")]
pub mod journald;
pub mod logbundle;
pub mod multiline;
pub mod parser;
/// The bounded per-line event ring + `@rpc/logs/events` queryable. Public so
/// the integration harness (#548) can drive socket→ring→query round-trips.
pub mod query;
pub mod receiver;
pub mod search;
pub mod sentinel;
pub mod store;
pub mod tls;

#[cfg(test)]
mod typed_subjects {
    use zensight_common::registry::logs::Subject;
    use zensight_common::subject::TelemetrySubject;

    /// The generated subjects render the tails this sensor published by hand
    /// (#1274): byte-identical keys for every legal name, so every consumer's
    /// series carries over. A unit name is slugged by the builder with RFC 03
    /// §2's injective escape — the hand-rolled map this replaces never folded
    /// case, so `NetworkManager.service` was a chunk the grammar refused.
    #[test]
    fn the_registered_families_render_their_tails() {
        for (subject, tail) in [
            (Subject::IngestReceivedTotal, "ingest/received_total"),
            (Subject::IngestDroppedRatio, "ingest/dropped_ratio"),
            (Subject::StoreOldestAgeSecs, "store/oldest_age_secs"),
            (Subject::by_severity("err_total"), "by_severity/err_total"),
            (Subject::ErrorsTotal, "errors_total"),
            (Subject::UnitsInFailure, "units_in_failure"),
            (
                Subject::JournaldSelfExcludedTotal,
                "journald/self_excluded_total",
            ),
            (
                Subject::by_unit_messages_total("nginx.service"),
                "by_unit/nginx.service/messages_total",
            ),
            (
                Subject::by_unit_burn_rate("other"),
                "by_unit/other/burn_rate",
            ),
            (
                Subject::by_template_count_total("a1b2c3"),
                "by_template/a1b2c3/count_total",
            ),
        ] {
            assert_eq!(subject.tail(), tail);
        }
        let chunk = zensight_sensor_core::key::device_chunk("NetworkManager.service");
        assert_eq!(
            Subject::by_unit_errors_total("NetworkManager.service").tail(),
            format!("by_unit/{}/errors_total", chunk.as_str())
        );
    }
}
