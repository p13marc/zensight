//! The explorer's payload-inspection pane (#748, the surface #791 needs).
//!
//! This is the thing the GUI never had (`zensight-common/src/schema.rs`'s
//! deferral note): somewhere that shows a key, its declared type, and the
//! bytes that actually arrived — what `zenctl` gives you on the CLI. The
//! bytes are read from the monitor's retention ring, so the pane is honest
//! about its scope: the latest *retained* sample, watched keys only.

use iced::widget::{column, row, text};
use iced::{Element, Font};

use zenkey_fleet::{SampleView, StampProvenance};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// Longest pretty-printed payload preview, in bytes. Bigger payloads are
/// truncated with the true size stated beside the preview — the size is a
/// fact, the preview is a courtesy.
const PREVIEW_CAP: usize = 4096;
/// Bytes shown by the hex fallback.
const HEX_CAP: usize = 256;

/// How the payload is previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// Decoded (CBOR/JSON first-byte sniff) and pretty-printed, possibly
    /// truncated at [`PREVIEW_CAP`].
    Json(String),
    /// Not decodable as a structural value: leading bytes, hex-dumped.
    Hex(String),
    /// A tombstone — a delete carries no payload, and rendering an empty
    /// preview would read as an empty put.
    Tombstone,
}

/// Everything the pane states about one retained sample. Built off the GUI
/// thread (the pump), carried by value in the tick snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedSample {
    /// The key, as this session sees it (base-relative under a namespaced
    /// deployment — the caption says so rather than claiming "wire key").
    pub key: String,
    pub delete: bool,
    pub encoding: String,
    /// True payload size — stated even when the preview truncates.
    pub payload_bytes: usize,
    /// The registry's declared payload type, when the key is registered.
    pub declared_type: Option<&'static str>,
    /// The registry's declared QoS profile name, when registered.
    pub declared_qos: Option<&'static str>,
    /// The wire's actual axes token.
    pub observed_qos: String,
    /// All-four-axes comparison against the declared profile; `None` when
    /// there is nothing declared to compare with — absent, not a pass.
    pub qos_ok: Option<bool>,
    /// Publisher stamp provenance, rendered.
    pub stamp: String,
    pub preview: Preview,
}

impl InspectedSample {
    /// Read one retained sample into the pane's facts. `declared_type` comes
    /// from the caller's registry memo (`ExplorerCore::declared_type`).
    pub fn of(view: &SampleView, declared_type: Option<&'static str>) -> InspectedSample {
        let delete = view.kind == zenoh::sample::SampleKind::Delete;
        let payload = view.payload.to_bytes();
        let preview = if delete {
            Preview::Tombstone
        } else {
            match zensight_common::decode_auto::<serde_json::Value>(&payload) {
                Ok(v) => {
                    let mut s = serde_json::to_string_pretty(&v).unwrap_or_default();
                    if s.len() > PREVIEW_CAP {
                        s.truncate(PREVIEW_CAP);
                        s.push('…');
                    }
                    Preview::Json(s)
                }
                Err(_) => {
                    let shown = &payload[..payload.len().min(HEX_CAP)];
                    let mut s = String::with_capacity(shown.len() * 3);
                    for (i, b) in shown.iter().enumerate() {
                        if i > 0 {
                            s.push(if i % 16 == 0 { '\n' } else { ' ' });
                        }
                        s.push_str(&format!("{b:02x}"));
                    }
                    if payload.len() > HEX_CAP {
                        s.push('…');
                    }
                    Preview::Hex(s)
                }
            }
        };
        let (declared_qos, qos_ok) = match super::core::declared_profile(&view.key) {
            Some(profile) => (Some(profile.name()), Some(view.qos_matches(profile))),
            None => (None, None),
        };
        InspectedSample {
            key: view.key.clone(),
            delete,
            encoding: view.encoding.clone(),
            payload_bytes: payload.len(),
            declared_type,
            declared_qos,
            observed_qos: super::core::axes_token(view),
            qos_ok,
            stamp: match view.stamped_by {
                None => "unstamped".to_string(),
                Some(StampProvenance::SelfStamped) => "self-stamped".to_string(),
                Some(StampProvenance::Foreign { .. }) => "stamped by a foreign HLC".to_string(),
                Some(StampProvenance::Unattributable { .. }) => {
                    "stamped, stamper unattributable".to_string()
                }
            },
            preview,
        }
    }
}

/// A `label: value` fact line.
fn fact<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label).size(font::CAPTION).width(110),
        text(value).size(font::CAPTION).font(Font::MONOSPACE),
    ]
    .spacing(space::SM)
    .into()
}

/// The pane. Pure render of an [`InspectedSample`].
pub fn inspector_pane(sample: &InspectedSample) -> Element<'_, Message> {
    let mut col = column![
        text("latest retained sample — watched keys only").size(font::CAPTION),
        text(sample.key.clone())
            .size(font::CAPTION)
            .font(Font::MONOSPACE),
    ]
    .spacing(space::XS);

    col = col.push(fact(
        "kind",
        if sample.delete {
            "delete (tombstone)"
        } else {
            "put"
        }
        .to_string(),
    ));
    col = col.push(fact(
        "declared type",
        sample
            .declared_type
            .map(str::to_string)
            .unwrap_or_else(|| "unregistered".to_string()),
    ));
    col = col.push(fact("encoding", sample.encoding.clone()));
    col = col.push(fact("payload", format!("{} bytes", sample.payload_bytes)));
    col = col.push(fact("observed qos", sample.observed_qos.clone()));
    col = col.push(fact(
        "declared qos",
        match (sample.declared_qos, sample.qos_ok) {
            (Some(name), Some(true)) => format!("{name} — matches"),
            (Some(name), Some(false)) => format!("{name} — MISMATCH"),
            (Some(name), None) => format!("{name}?"),
            (None, _) => "none declared".to_string(),
        },
    ));
    col = col.push(fact("stamp", sample.stamp.clone()));

    let preview: Element<'_, Message> = match &sample.preview {
        Preview::Tombstone => text("(tombstone — no payload)").size(font::CAPTION).into(),
        Preview::Json(s) | Preview::Hex(s) => {
            iced::widget::scrollable(text(s.clone()).size(font::CAPTION).font(Font::MONOSPACE))
                .height(iced::Length::Fill)
                .into()
        }
    };
    col = col.push(preview);

    crate::view::components::kit::card(col.spacing(space::XS))
}
