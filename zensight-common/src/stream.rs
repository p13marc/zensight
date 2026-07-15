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

/// One named bandwidth **tier** a stream offers — the `<tier>` key chunk plus
/// its target encoder parameters (RFC 07 §1). Tiers are published concurrently,
/// each on its own `@media/<stream>/video/<codec>/<tier>` key; a viewer
/// subscribes to exactly the tier its link can take, so two viewers on
/// different links never fight over one encoder (#494, #497).
///
/// This is the tier *definition* — the sensor owns the numbers; the wire and
/// the key carry the *name*. It appears in [`StreamDescriptor::tiers`] (the
/// catalogue) and in the `tiers/set` admin command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierSpec {
    /// Tier name — the `<tier>` key chunk (`low` / `medium` / `high`).
    pub name: String,
    /// Aspect-preserving height cap in pixels; `None` = native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    /// Target framerate.
    pub fps: u32,
    /// Target encoded bitrate.
    pub bitrate_kbps: u32,
}

/// Runtime control for one media stream, sent on the `stream` command topic.
///
/// Tagged like the other sensor command enums (`type`, snake_case), so on the
/// wire an open looks like
/// `{"type":"open_stream","stream":"cam0","tier":"high"}`.
///
/// Note per-viewer quality is expressed by *which `<tier>` key you subscribe
/// to*, not by a command (#494). These commands manage a stream's lifecycle and
/// keyframes; redefining what a tier *means* is a separate `tiers/set` admin
/// command ([`TierSpec`]), the only global knob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamControl {
    /// Open (start publishing) one tier of a stream. The sensor declares the
    /// media publisher on the concrete `@media/<stream>/video/<codec>/<tier>`
    /// key (or `…/preview/<format>` for the preview codec) and starts the
    /// pipeline. Distinct tiers open independent encoders.
    OpenStream {
        /// Stream identifier (the `<stream>` key chunk).
        stream: String,
        /// Requested codec (e.g. `h264`, `mjpeg` for the preview); `None` =
        /// sensor default (video).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// Which tier to open (the `<tier>` key chunk); `None` = the sensor's
        /// default tier. Ignored for the preview codec.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
    /// Close (stop publishing) one tier of a stream and undeclare its media
    /// publisher. Mirrors the `OpenStream` selector.
    CloseStream {
        /// Stream identifier.
        stream: String,
        /// Codec, matching the open; `None` = video default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// Tier, matching the open; `None` = default tier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
    /// Force the encoder to emit a keyframe (IDR) on the next access unit of one
    /// tier. Normally unnecessary — the matching listener forces a keyframe when
    /// a subscriber appears — but RFC 07 §1 mandates it for the Nth viewer, who
    /// gets no matching-listener edge.
    RequestKeyframe {
        /// Stream identifier.
        stream: String,
        /// Which tier's encoder to force; `None` = default tier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
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
///
/// **Capability-bearing** (#507): a viewer builds a sensible tier selector from
/// the camera's *native* geometry and the tiers on offer, without opening the
/// stream first. Native `width`/`height`/`fps` are probed from the source
/// (`None` when genuinely unknown — e.g. an RTSP stream whose SDP carries no
/// dimensions); `codecs` reflects the real per-source capability, not a
/// hardcoded pair.
// No `Eq`: `fps` is an `f32` (native framerate), which is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    /// Stream identifier (the `<stream>` key chunk).
    pub stream: String,
    /// Codecs this stream can be opened with (e.g. `["h264", "mjpeg"]`).
    pub codecs: Vec<String>,
    /// Whether the stream is currently open (any tier publishing).
    pub active: bool,
    /// Native capture width in pixels; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Native capture height in pixels; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Native capture framerate; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f32>,
    /// The bandwidth tiers this stream offers, so a viewer can subscribe to an
    /// exact `<tier>` key and never advertise a tier the camera can't feed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierSpec>,
    /// Optional human-readable description (camera position, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The **applied** parameters of one running tier — actual, not requested (a
/// hardware encoder may silently ignore a knob, and the scaler even-aligns
/// dimensions). This is what the GUI should render as the tile's real state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierApplied {
    /// Encoded width in pixels (post-scale, even-aligned).
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Applied framerate cap.
    pub fps: u32,
    /// Applied encoded bitrate.
    pub bitrate_kbps: u32,
}

/// State of one running tier, reported inside [`StreamStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierStatus {
    /// Tier name (the `<tier>` key chunk).
    pub tier: String,
    /// Parameters actually in effect on this tier's encoder.
    pub applied: TierApplied,
    /// Matching subscribers observed by this tier's media publisher.
    pub viewers: u32,
}

/// Current state of one stream, reported on the `stream/<stream>` status doc.
///
/// **Per-tier** (#497): a stream can have several tiers live at once, each with
/// its own applied params and viewer count. A single `Option<profile>` could
/// not express two live tiers — this is a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStatus {
    /// Stream identifier.
    pub stream: String,
    /// Whether the stream is currently open (any tier publishing).
    pub open: bool,
    /// Per-tier state for every tier currently live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierStatus>,
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
                tier: Some("high".into()),
            },
            StreamControl::OpenStream {
                stream: "cam1".into(),
                codec: None,
                tier: None,
            },
            StreamControl::CloseStream {
                stream: "cam0".into(),
                codec: Some("h264".into()),
                tier: Some("low".into()),
            },
            StreamControl::RequestKeyframe {
                stream: "cam0".into(),
                tier: Some("medium".into()),
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
            tier: None,
        })
        .unwrap();
        assert_eq!(json["type"], "open_stream");
        assert_eq!(json["stream"], "cam0");
        assert_eq!(json["codec"], "h264");
        assert!(json.get("tier").is_none(), "None fields are omitted");

        let json = serde_json::to_value(StreamControl::RequestKeyframe {
            stream: "cam0".into(),
            tier: None,
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
            width: Some(1280),
            height: Some(720),
            fps: Some(30.0),
            tiers: vec![
                TierSpec {
                    name: "low".into(),
                    max_height: Some(240),
                    fps: 10,
                    bitrate_kbps: 400,
                },
                TierSpec {
                    name: "high".into(),
                    max_height: None,
                    fps: 30,
                    bitrate_kbps: 4000,
                },
            ],
            description: Some("front door".into()),
        };
        let bytes = encode(&desc, Format::Cbor).unwrap();
        let back: StreamDescriptor = decode(&bytes, Format::Cbor).unwrap();
        assert_eq!(back, desc);

        let status = StreamStatus {
            stream: "cam0".into(),
            open: true,
            tiers: vec![
                TierStatus {
                    tier: "low".into(),
                    applied: TierApplied {
                        width: 320,
                        height: 240,
                        fps: 10,
                        bitrate_kbps: 400,
                    },
                    viewers: 1,
                },
                TierStatus {
                    tier: "high".into(),
                    applied: TierApplied {
                        width: 1280,
                        height: 720,
                        fps: 30,
                        bitrate_kbps: 4000,
                    },
                    viewers: 2,
                },
            ],
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
            codec: None,
            tier: None,
        })
        .with_id("req-1");
        let bytes = encode(&cmd, Format::Json).unwrap();
        let back: Command<StreamControl> = decode(&bytes, Format::Json).unwrap();
        assert_eq!(back.id.as_deref(), Some("req-1"));
        assert_eq!(
            back.body,
            StreamControl::CloseStream {
                stream: "cam0".into(),
                codec: None,
                tier: None,
            }
        );
    }
}
