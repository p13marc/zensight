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

/// A sequence number that regresses by at least this much is a sensor-side
/// pipeline restart (its per-stream counter reset to ~0), not a reordered
/// frame: within one subscription, frames are decoded and applied in arrival
/// order, so genuine reordering spans at most a handful of frames — while a
/// restarted pipeline jumps back by whatever the old pipeline had counted up
/// to. 300 frames (≥ 10 s of video, ≥ 1 min of previews) is far beyond any
/// reorder window, so the guard re-anchors instead of freezing the tile.
const SEQ_RESTART_GAP: u64 = 300;

/// Per-device parallax state: the stream catalogue + open preview tiles.
#[derive(Debug, Default)]
pub struct ParallaxDetailState {
    /// The advertised streams (`@/query/streams`).
    pub catalogue: Fetch<Vec<StreamDescriptor>>,
    /// Open preview tiles, keyed by stream name (BTreeMap: stable grid order).
    pub tiles: BTreeMap<String, TileState>,
    /// Generation source for [`Self::allocate_generation`].
    next_generation: u64,
}

/// One live preview tile.
#[derive(Debug)]
pub struct TileState {
    /// Which tile incarnation this is (see [`ParallaxDetailState::allocate_generation`]):
    /// frames and end reports from a replaced subscriber task carry an older
    /// generation and are ignored — a stale `ParallaxTileEnded` must never
    /// clear the NEW tile's abort handle, and a stale frame must never leak
    /// its (differently-domained) sequence number into this tile.
    pub generation: u64,
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
    fn new(generation: u64, abort: Option<iced::task::Handle>) -> Self {
        Self {
            generation,
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

    /// Allocate the generation for a tile that is about to open (monotonic
    /// per state). The caller passes it to the subscriber stream AND to
    /// [`Self::open_tile`], so late messages from a replaced task (older
    /// generation) can be told apart from the live one.
    pub fn allocate_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Register an opened tile (replacing — and thereby aborting — any
    /// previous tile for the stream).
    pub fn open_tile(&mut self, stream: &str, generation: u64, abort: Option<iced::task::Handle>) {
        self.tiles
            .insert(stream.to_string(), TileState::new(generation, abort));
    }

    /// Fold a decoded frame in. Returns `false` (frame ignored) when the
    /// tile is gone, the frame belongs to a replaced tile incarnation, or
    /// the sequence number is stale — with latest-wins draining a reordered
    /// older frame must never replace a newer one. A sequence that fell back
    /// by [`SEQ_RESTART_GAP`] or more is a sensor pipeline restart within
    /// the same subscription: accept it and re-anchor `last_seq` instead of
    /// freezing the tile.
    pub fn apply_frame(
        &mut self,
        stream: &str,
        generation: u64,
        seq: u64,
        handle: image::Handle,
    ) -> bool {
        let Some(tile) = self.tiles.get_mut(stream) else {
            return false;
        };
        if tile.generation != generation {
            return false;
        }
        if tile.frame.is_some() && seq <= tile.last_seq && tile.last_seq - seq < SEQ_RESTART_GAP {
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
    /// End reports from a replaced tile incarnation are ignored — clearing
    /// the NEW tile's abort handle here would leak (and orphan) its live
    /// subscriber task.
    pub fn end_tile(&mut self, stream: &str, generation: u64, error: Option<String>) {
        if let Some(tile) = self.tiles.get_mut(stream)
            && tile.generation == generation
        {
            tile.abort = None;
            tile.ended = Some(error.unwrap_or_else(|| "stream ended".to_string()));
        }
    }

    /// Fold a sensor-side `StreamStatus` transition (`@/status/streams`) in:
    /// a definitive `open: false` for a tile still waiting for its first
    /// frame means the open failed on the sensor — surface it instead of
    /// "waiting for frames…" forever. The subscriber task stays alive (the
    /// hint self-heals if frames do arrive later: `apply_frame` clears it).
    pub fn apply_stream_status(&mut self, status: &zensight_common::stream::StreamStatus) {
        if status.open {
            return;
        }
        if let Some(tile) = self.tiles.get_mut(&status.stream)
            && tile.frame.is_none()
            && tile.ended.is_none()
        {
            tile.ended = Some("stream failed to open on the sensor".to_string());
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
/// wrapping task drops the future and undeclares the subscriber. Every
/// yielded message carries the tile `generation` this task was opened with.
pub fn preview_tile_stream(
    session: Arc<Session>,
    source: String,
    stream: String,
    generation: u64,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        // The EXACT preview key — never a wildcard (pinned in tests).
        let key = media_preview_key(Protocol::Parallax, &source, &stream);
        let subscriber = match session.declare_subscriber(&key).await {
            Ok(s) => s,
            Err(e) => {
                yield Message::ParallaxTileEnded {
                    stream,
                    generation,
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
                    generation,
                    seq: meta.sequence,
                    handle,
                };
            }
        }
        yield Message::ParallaxTileEnded {
            stream,
            generation,
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
        let generation = state.allocate_generation();
        state.open_tile("cam0", generation, None);

        assert!(state.apply_frame("cam0", generation, 5, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 5);

        // Newer sequence replaces…
        assert!(state.apply_frame("cam0", generation, 6, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 6);

        // …stale (reordered) sequence is dropped…
        assert!(!state.apply_frame("cam0", generation, 4, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 6);

        // …and frames for unknown tiles are ignored.
        assert!(!state.apply_frame("nope", generation, 1, dummy_handle()));
    }

    #[test]
    fn stale_generation_frames_are_ignored() {
        let mut state = ParallaxDetailState::default();
        let old = state.allocate_generation();
        state.open_tile("cam0", old, None);
        assert!(state.apply_frame("cam0", old, 500, dummy_handle()));

        // Replace the tile (e.g. preview → video switch): a leftover frame
        // from the old subscriber must not land on the new tile — its
        // sequence domain (500…) would freeze the new tile at seq 0….
        let new = state.allocate_generation();
        state.open_tile("cam0", new, None);
        assert!(!state.apply_frame("cam0", old, 501, dummy_handle()));
        assert!(state.tiles["cam0"].frame.is_none());

        // The new incarnation's frames apply from its own domain.
        assert!(state.apply_frame("cam0", new, 1, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 1);
    }

    #[test]
    fn restart_regression_reanchors_within_a_generation() {
        let mut state = ParallaxDetailState::default();
        let generation = state.allocate_generation();
        state.open_tile("cam0", generation, None);
        assert!(state.apply_frame("cam0", generation, 5_000, dummy_handle()));

        // A small regression is reordering: dropped.
        assert!(!state.apply_frame("cam0", generation, 4_990, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 5_000);

        // A huge regression is a sensor pipeline restart (sequence reset to
        // ~0) within the same subscription: accepted and re-anchored.
        assert!(state.apply_frame("cam0", generation, 3, dummy_handle()));
        assert_eq!(state.tiles["cam0"].last_seq, 3);
        assert!(state.apply_frame("cam0", generation, 4, dummy_handle()));
    }

    #[test]
    fn stale_tile_ended_does_not_kill_the_new_tile() {
        let mut state = ParallaxDetailState::default();
        let old = state.allocate_generation();
        state.open_tile("cam0", old, None);
        let new = state.allocate_generation();
        state.open_tile("cam0", new, None);

        // The replaced (aborted) subscriber's end report arrives late: it
        // must neither clear the new tile's abort handle nor mark it ended.
        state.end_tile("cam0", old, Some("aborted".into()));
        assert!(state.tiles["cam0"].ended.is_none());

        // The live incarnation's end report still applies.
        state.end_tile("cam0", new, None);
        assert_eq!(state.tiles["cam0"].ended.as_deref(), Some("stream ended"));
    }

    #[test]
    fn stream_status_marks_waiting_tiles_failed() {
        use zensight_common::stream::StreamStatus;
        let mut state = ParallaxDetailState::default();
        let generation = state.allocate_generation();
        state.open_tile("cam0", generation, None);

        // open: true transitions never touch the tile.
        state.apply_stream_status(&StreamStatus {
            stream: "cam0".into(),
            open: true,
            viewers: 1,
            profile: Some("mjpeg".into()),
        });
        assert!(state.tiles["cam0"].ended.is_none());

        // A definitive open: false while still waiting = failed open.
        let closed = StreamStatus {
            stream: "cam0".into(),
            open: false,
            viewers: 0,
            profile: None,
        };
        state.apply_stream_status(&closed);
        assert!(
            state.tiles["cam0"]
                .ended
                .as_deref()
                .unwrap()
                .contains("failed to open")
        );

        // Self-healing: a frame that does arrive clears the hint…
        assert!(state.apply_frame("cam0", generation, 1, dummy_handle()));
        assert!(state.tiles["cam0"].ended.is_none());

        // …and a tile that already shows frames is never flipped by a
        // (teardown-driven) open: false transition.
        state.apply_stream_status(&closed);
        assert!(state.tiles["cam0"].ended.is_none());
    }

    #[test]
    fn end_and_close_lifecycle() {
        let mut state = ParallaxDetailState::default();
        let g0 = state.allocate_generation();
        state.open_tile("cam0", g0, None);
        let g1 = state.allocate_generation();
        state.open_tile("cam1", g1, None);
        assert!(state.is_open("cam0"));

        state.end_tile("cam0", g0, Some("boom".into()));
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
