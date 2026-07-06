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
