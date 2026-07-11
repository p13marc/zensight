//! Media stream-control types for the `@media` plane (#359).
//!
//! Only the **pixels** ride `@media`
//! ([`crate::keyexpr::media_video_key`] / [`crate::keyexpr::media_preview_key`],
//! opaque bytes, no serialization envelope). Stream *control* rides the
//! ordinary `@/` channels built with [`crate::command`] helpers:
//!
//! - commands: `command_key(prefix, "stream")` carries a
//!   [`Command`](crate::command::Command)<[`StreamControl`]>
//! - query: `query_key(prefix, "streams")` lists the advertised
//!   [`StreamDescriptor`]s (queryable, late-joiner seed)
//! - status: `status_key(prefix, "streams")` reports per-stream
//!   [`StreamStatus`] (open sessions / active profile / viewers)
//!
//! Stream *stats* (fps/kbps/drops/viewers) ride normal telemetry under
//! `zensight/<proto>/<source>/<stream>/stats/<metric>` so existing charts light
//! up for free.

use serde::{Deserialize, Serialize};

/// Runtime control for one media stream, sent on the `stream` command topic.
///
/// Tagged like the other sensor command enums (`type`, snake_case), so on the
/// wire an open looks like
/// `{"type":"open_stream","stream":"cam0","codec":"h264"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamControl {
    /// Open (start publishing) a stream. The sensor declares the media
    /// publisher on the concrete `@media/<stream>/…` key and starts the
    /// pipeline.
    OpenStream {
        /// Stream identifier (the `<stream>` key chunk).
        stream: String,
        /// Requested codec (e.g. `h264`); `None` = sensor default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// Cap the encoded height in pixels (bandwidth control); `None` = native.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_height: Option<u32>,
    },
    /// Close (stop publishing) a stream and undeclare its media publisher.
    CloseStream {
        /// Stream identifier.
        stream: String,
    },
    /// Force the encoder to emit a keyframe (IDR) on the next access unit.
    /// Normally unnecessary — the sensor's matching listener already forces a
    /// keyframe when a subscriber appears — but exposed for explicit recovery.
    RequestKeyframe {
        /// Stream identifier.
        stream: String,
    },
}

/// Per-frame metadata riding as the Zenoh **attachment** on every `@media`
/// sample (#403). The payload stays opaque encoded bytes; this sidecar is what
/// lets a viewer gate on keyframes, detect gaps, and time frames without
/// parsing the bitstream.
///
/// Encoded with [`crate::serialization::encode`] as **CBOR** — it is *not* a
/// telemetry envelope (`TelemetryPoint`/`Format` never appear on `@media`),
/// just a small struct serialized compactly. `None` timing fields are omitted
/// on the wire (the encoder had no clock for them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FrameMeta {
    /// Whether this frame is independently decodable (H.264 IDR / any JPEG).
    pub keyframe: bool,
    /// Presentation timestamp in nanoseconds, if the pipeline stamped one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pts_ns: Option<u64>,
    /// Decode timestamp in nanoseconds, if distinct from `pts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dts_ns: Option<u64>,
    /// Frame duration in nanoseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<u64>,
    /// Monotonic per-stream sequence number (gap ⇒ dropped frames).
    pub sequence: u64,
    /// Encoded frame width in pixels.
    pub width: u32,
    /// Encoded frame height in pixels.
    pub height: u32,
}

/// One advertised media stream, served from the `streams` query topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    /// Stream identifier (the `<stream>` key chunk).
    pub stream: String,
    /// Codecs this stream can be opened with (e.g. `["h264", "mjpeg"]`).
    pub codecs: Vec<String>,
    /// Whether the stream is currently open (publishing).
    pub active: bool,
    /// Optional human-readable description (camera position, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Current state of one stream, reported on the `streams` status topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStatus {
    /// Stream identifier.
    pub stream: String,
    /// Whether the stream is currently open (publishing).
    pub open: bool,
    /// Number of matching subscribers observed by the media publisher.
    pub viewers: u32,
    /// Active `<codec>/<profile>` when open (e.g. `h264/main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::{Format, decode, encode};

    #[test]
    fn stream_control_roundtrip_all_variants() {
        for control in [
            StreamControl::OpenStream {
                stream: "cam0".into(),
                codec: Some("h264".into()),
                max_height: Some(720),
            },
            StreamControl::OpenStream {
                stream: "cam1".into(),
                codec: None,
                max_height: None,
            },
            StreamControl::CloseStream {
                stream: "cam0".into(),
            },
            StreamControl::RequestKeyframe {
                stream: "cam0".into(),
            },
        ] {
            for format in [Format::Json, Format::Cbor] {
                let bytes = encode(&control, format).unwrap();
                let back: StreamControl = decode(&bytes, format).unwrap();
                assert_eq!(back, control);
            }
        }
    }

    #[test]
    fn stream_control_wire_tag_convention() {
        // Same `type`-tagged snake_case shape as the other command enums.
        let json = serde_json::to_value(StreamControl::OpenStream {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            max_height: None,
        })
        .unwrap();
        assert_eq!(json["type"], "open_stream");
        assert_eq!(json["stream"], "cam0");
        assert_eq!(json["codec"], "h264");
        assert!(json.get("max_height").is_none(), "None fields are omitted");

        let json = serde_json::to_value(StreamControl::RequestKeyframe {
            stream: "cam0".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "request_keyframe");
    }

    #[test]
    fn descriptor_and_status_roundtrip() {
        let desc = StreamDescriptor {
            stream: "cam0".into(),
            codecs: vec!["h264".into(), "mjpeg".into()],
            active: true,
            description: Some("front door".into()),
        };
        let bytes = encode(&desc, Format::Cbor).unwrap();
        let back: StreamDescriptor = decode(&bytes, Format::Cbor).unwrap();
        assert_eq!(back, desc);

        let status = StreamStatus {
            stream: "cam0".into(),
            open: true,
            viewers: 2,
            profile: Some("h264/main".into()),
        };
        let bytes = encode(&status, Format::Json).unwrap();
        let back: StreamStatus = decode(&bytes, Format::Json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn frame_meta_roundtrip_both_formats() {
        for meta in [
            FrameMeta {
                keyframe: true,
                pts_ns: Some(1_000_000_000),
                dts_ns: Some(999_000_000),
                duration_ns: Some(33_333_333),
                sequence: 42,
                width: 1280,
                height: 720,
            },
            FrameMeta {
                keyframe: false,
                pts_ns: None,
                dts_ns: None,
                duration_ns: None,
                sequence: 0,
                width: 320,
                height: 240,
            },
        ] {
            for format in [Format::Json, Format::Cbor] {
                let bytes = encode(&meta, format).unwrap();
                let back: FrameMeta = decode(&bytes, format).unwrap();
                assert_eq!(back, meta);
            }
        }
    }

    #[test]
    fn frame_meta_none_timing_fields_are_omitted() {
        // Pin the wire shape: absent timing must not serialize as nulls.
        let json = serde_json::to_value(FrameMeta {
            keyframe: true,
            pts_ns: None,
            dts_ns: None,
            duration_ns: None,
            sequence: 7,
            width: 640,
            height: 360,
        })
        .unwrap();
        assert_eq!(json["keyframe"], true);
        assert_eq!(json["sequence"], 7);
        assert_eq!(json["width"], 640);
        assert_eq!(json["height"], 360);
        assert!(json.get("pts_ns").is_none(), "None fields are omitted");
        assert!(json.get("dts_ns").is_none());
        assert!(json.get("duration_ns").is_none());
    }

    #[test]
    fn stream_control_in_command_envelope() {
        use crate::command::Command;
        let cmd = Command::new(StreamControl::CloseStream {
            stream: "cam0".into(),
        })
        .with_id("req-1");
        let bytes = encode(&cmd, Format::Json).unwrap();
        let back: Command<StreamControl> = decode(&bytes, Format::Json).unwrap();
        assert_eq!(back.id.as_deref(), Some("req-1"));
        assert_eq!(
            back.body,
            StreamControl::CloseStream {
                stream: "cam0".into()
            }
        );
    }
}
