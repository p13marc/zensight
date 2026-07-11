//! Parallax stream catalogue + live preview tiles — state and transport
//! (#408, epic #402).
//!
//! The catalogue is an on-demand [`Fetch`] from the sensor's host-scoped
//! `@/query/streams` queryable. Each opened tile runs one abortable
//! [`iced::Task::stream`] built from [`preview_tile_stream`]: a plain Zenoh
//! subscriber on the exact `@media/<stream>/preview/jpeg` key, draining to
//! the newest frame (latest wins — stale previews are worthless), decoding
//! the CBOR [`FrameMeta`] attachment, and JPEG→RGBA off the UI thread.
//! Aborting the task drops the future, which undeclares the subscriber —
//! the sensor's matching listener sees the falling edge and (with the
//! `close_stream` we also send) reaps the pipeline.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use iced::futures::Stream;
use iced::widget::image;
use zenoh::Session;
use zensight_common::command::query_key;
use zensight_common::keyexpr::media_preview_key;
use zensight_common::stream::{FrameMeta, StreamDescriptor};
use zensight_common::{Format, Protocol, decode};

use super::fetch::Fetch;
use super::parallax::preview_handle_from_jpeg;
use crate::message::Message;

/// Smoothing factor for the tile fps EMA (per frame).
const FPS_EMA_ALPHA: f32 = 0.2;

/// Per-device parallax state: the stream catalogue + open preview tiles.
#[derive(Debug, Default)]
pub struct ParallaxDetailState {
    /// The advertised streams (`@/query/streams`).
    pub catalogue: Fetch<Vec<StreamDescriptor>>,
    /// Open preview tiles, keyed by stream name (BTreeMap: stable grid order).
    pub tiles: BTreeMap<String, TileState>,
}

/// One live preview tile.
#[derive(Debug)]
pub struct TileState {
    /// Newest decoded frame (None until the first frame arrives).
    pub frame: Option<image::Handle>,
    /// Sequence number of `frame` (stale frames are dropped).
    pub last_seq: u64,
    /// Exponential moving average of the arrival rate.
    pub fps: f32,
    /// When the last frame arrived (drives the fps EMA).
    pub last_frame_at: Option<Instant>,
    /// Abort handle for the subscriber task; aborts on drop as well
    /// (belt-and-braces — dropping the tile always kills the subscriber).
    pub abort: Option<iced::task::Handle>,
    /// Set when the stream ended (subscriber task finished); the tile shows
    /// the reason instead of a frame.
    pub ended: Option<String>,
}

impl TileState {
    fn new(abort: Option<iced::task::Handle>) -> Self {
        Self {
            frame: None,
            last_seq: 0,
            fps: 0.0,
            last_frame_at: None,
            abort,
            ended: None,
        }
    }
}

impl ParallaxDetailState {
    /// Mark the catalogue as loading (a fetch is in flight).
    pub fn loading(&mut self) {
        self.catalogue = Fetch::Loading;
    }

    /// Fold a catalogue reply in.
    pub fn apply(&mut self, result: Result<Vec<StreamDescriptor>, String>) {
        self.catalogue = Fetch::from_result(result);
    }

    /// Whether a tile for `stream` is open.
    pub fn is_open(&self, stream: &str) -> bool {
        self.tiles.contains_key(stream)
    }

    /// Register an opened tile (replacing — and thereby aborting — any
    /// previous tile for the stream).
    pub fn open_tile(&mut self, stream: &str, abort: Option<iced::task::Handle>) {
        self.tiles.insert(stream.to_string(), TileState::new(abort));
    }

    /// Fold a decoded frame in. Returns `false` (frame ignored) when the
    /// tile is gone or the sequence number is stale — with latest-wins
    /// draining a reordered older frame must never replace a newer one.
    pub fn apply_frame(&mut self, stream: &str, seq: u64, handle: image::Handle) -> bool {
        let Some(tile) = self.tiles.get_mut(stream) else {
            return false;
        };
        if tile.frame.is_some() && seq <= tile.last_seq {
            return false;
        }
        let now = Instant::now();
        if let Some(prev) = tile.last_frame_at {
            let dt = now.duration_since(prev).as_secs_f32();
            if dt > 0.0 {
                let instant_fps = 1.0 / dt;
                tile.fps = if tile.fps == 0.0 {
                    instant_fps
                } else {
                    tile.fps + FPS_EMA_ALPHA * (instant_fps - tile.fps)
                };
            }
        }
        tile.last_frame_at = Some(now);
        tile.last_seq = seq;
        tile.frame = Some(handle);
        tile.ended = None;
        true
    }

    /// The subscriber task for `stream` finished (error or clean end).
    pub fn end_tile(&mut self, stream: &str, error: Option<String>) {
        if let Some(tile) = self.tiles.get_mut(stream) {
            tile.abort = None;
            tile.ended = Some(error.unwrap_or_else(|| "stream ended".to_string()));
        }
    }

    /// Close one tile: abort its subscriber task and drop it.
    pub fn close_tile(&mut self, stream: &str) {
        if let Some(tile) = self.tiles.remove(stream)
            && let Some(abort) = tile.abort
        {
            abort.abort();
        }
    }

    /// Tear every tile down (device deselected / disconnected): abort all
    /// subscriber tasks, clear the map, and return the stream names so the
    /// caller can batch `close_stream` commands.
    pub fn teardown(&mut self) -> Vec<String> {
        let streams: Vec<String> = self.tiles.keys().cloned().collect();
        for (_, tile) in std::mem::take(&mut self.tiles) {
            if let Some(abort) = tile.abort {
                abort.abort();
            }
        }
        streams
    }
}

/// The sensor's host-scoped control prefix for `host`.
pub fn host_prefix(host: &str) -> String {
    format!("zensight/parallax/{host}")
}

/// Query the stream catalogue (`@/query/streams`) for `host`.
pub async fn fetch_streams(session: Arc<Session>, host: String) -> Option<Vec<StreamDescriptor>> {
    let key = query_key(&host_prefix(&host), "streams");
    let replies = session.get(key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

/// The per-tile subscriber stream: newest JPEG preview frames decoded to
/// [`image::Handle`]s. Ends with [`Message::ParallaxTileEnded`]; aborting the
/// wrapping task drops the future and undeclares the subscriber.
pub fn preview_tile_stream(
    session: Arc<Session>,
    source: String,
    stream: String,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        // The EXACT preview key — never a wildcard (pinned in tests).
        let key = media_preview_key(Protocol::Parallax, &source, &stream);
        let subscriber = match session.declare_subscriber(&key).await {
            Ok(s) => s,
            Err(e) => {
                yield Message::ParallaxTileEnded {
                    stream,
                    error: Some(format!("subscribe failed: {e}")),
                };
                return;
            }
        };
        loop {
            let mut sample = match subscriber.recv_async().await {
                Ok(s) => s,
                Err(_) => break, // session closed
            };
            // Latest frame wins: drain any backlog before decoding.
            while let Ok(Some(newer)) = subscriber.try_recv() {
                sample = newer;
            }
            let meta: FrameMeta = sample
                .attachment()
                .and_then(|a| decode(&a.to_bytes(), Format::Cbor).ok())
                .unwrap_or_default();
            let payload = sample.payload().to_bytes().to_vec();
            // JPEG→RGBA decode off the UI thread.
            let decoded =
                tokio::task::spawn_blocking(move || preview_handle_from_jpeg(&payload)).await;
            if let Ok(Some(handle)) = decoded {
                yield Message::ParallaxFrame {
                    stream: stream.clone(),
                    seq: meta.sequence,
                    handle,
                };
            }
        }
        yield Message::ParallaxTileEnded {
            stream,
            error: None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_handle() -> image::Handle {
        image::Handle::from_rgba(2, 2, vec![0u8; 16])
    }

    #[test]
    fn preview_key_is_verbatim_media_plane() {
        // Pin the exact key the tile stream subscribes to — the sensor
        // publishes on this literal key (cross-crate contract).
        assert_eq!(
            media_preview_key(Protocol::Parallax, "hostA", "cam0"),
            "zensight/parallax/hostA/@media/cam0/preview/jpeg"
        );
        assert_eq!(host_prefix("hostA"), "zensight/parallax/hostA");
    }

    #[test]
    fn frames_replace_and_stale_sequences_drop() {
        let mut state = ParallaxDetailState::default();
        state.open_tile("cam0", None);

        assert!(state.apply_frame("cam0", 5, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 5);

        // Newer sequence replaces…
        assert!(state.apply_frame("cam0", 6, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 6);

        // …stale (reordered) sequence is dropped…
        assert!(!state.apply_frame("cam0", 4, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 6);

        // …and frames for unknown tiles are ignored.
        assert!(!state.apply_frame("nope", 1, dummy_handle()));
    }

    #[test]
    fn end_and_close_lifecycle() {
        let mut state = ParallaxDetailState::default();
        state.open_tile("cam0", None);
        state.open_tile("cam1", None);
        assert!(state.is_open("cam0"));

        state.end_tile("cam0", Some("boom".into()));
        assert_eq!(state.tiles["cam0"].ended.as_deref(), Some("boom"));

        state.close_tile("cam0");
        assert!(!state.is_open("cam0"));

        let torn = state.teardown();
        assert_eq!(torn, vec!["cam1".to_string()]);
        assert!(state.tiles.is_empty());
    }

    #[test]
    fn catalogue_fetch_lifecycle() {
        let mut state = ParallaxDetailState::default();
        assert!(matches!(state.catalogue, Fetch::Idle));
        state.loading();
        assert!(state.catalogue.is_loading());
        state.apply(Ok(vec![StreamDescriptor {
            stream: "cam0".into(),
            codecs: vec!["h264".into(), "mjpeg".into()],
            active: false,
            description: None,
        }]));
        assert_eq!(state.catalogue.ready().map(|v| v.len()), Some(1));
        state.apply(Err("no sensor".into()));
        assert_eq!(state.catalogue.error(), Some("no sensor"));
    }
}
