//! Parallax stream catalogue + live preview tiles — state and transport
//! (#408, epic #402).
//!
//! The catalogue is an on-demand [`Fetch`] from the sensor's origin-scoped
//! `@rpc/parallax/streams` queryable. Each opened tile runs one abortable
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
use zensight_common::keyexpr::{media_preview_key, origin_rpc_key};
use zensight_common::stream::{FrameMeta, StreamControl, StreamDescriptor, StreamStatus, TierSpec};
use zensight_common::{Format, decode};

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
    /// The advertised streams (`@rpc/parallax/streams`).
    pub catalogue: Fetch<Vec<StreamDescriptor>>,
    /// Open preview tiles, keyed by stream name (BTreeMap: stable grid order).
    pub tiles: BTreeMap<String, TileState>,
    /// Latest per-stream `StreamStatus` (`state/parallax/stream/<stream>`),
    /// keyed by stream. Carries each open tier's **applied** params
    /// (resolution/fps/bitrate) + viewer count — the honest bandwidth readout
    /// the tile shows (#503), sourced from the sensor rather than a client-side
    /// arrival EMA.
    pub status: BTreeMap<String, StreamStatus>,
    /// The tile currently shown in the near-fullscreen overlay (#436).
    /// Lives on this state (not the app) so every existing teardown choke
    /// point that clears the tiles also dismisses the overlay.
    pub expanded: Option<ExpandedTile>,
    /// Generation source for [`Self::allocate_generation`].
    next_generation: u64,
}

/// The expanded-tile overlay (#436): which stream fills the screen, and what
/// profile the tile ran *before* expanding — collapsing restores it, so the
/// sensor-side refcounts return to their pre-expand state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedTile {
    /// Stream name (tile key).
    pub stream: String,
    /// Whether the tile was already on the H.264 video profile when it was
    /// expanded (`false` = preview tile that expand upgraded, collapse
    /// downgrades back).
    pub was_video: bool,
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
    /// Whether this tile runs the H.264 video profile (`false` = JPEG
    /// preview). Set at open; the expand overlay uses it to decide whether
    /// to upgrade and what to restore on collapse (#436).
    pub video: bool,
    /// For a video tile, the exact `<tier>` it subscribes to and opened on
    /// the sensor (#494/#502). `None` for a JPEG preview tile (previews have
    /// no tier). Every close/keyframe for this tile must carry it so the
    /// sensor decrements the *right* per-tier refcount.
    pub selected_tier: Option<String>,
}

impl TileState {
    fn new(
        generation: u64,
        abort: Option<iced::task::Handle>,
        video: bool,
        selected_tier: Option<String>,
    ) -> Self {
        Self {
            generation,
            frame: None,
            last_seq: 0,
            fps: 0.0,
            last_frame_at: None,
            abort,
            ended: None,
            video,
            selected_tier,
        }
    }

    /// The `CloseStream` that reaps exactly this tile's profile. A codec-less
    /// close would resolve to the sensor's *default video tier* (see the
    /// sensor's `resolve_profile`), so a preview or a non-default tier must
    /// name its own codec/tier or the wrong refcount is decremented.
    pub fn close_control(&self, stream: &str) -> StreamControl {
        if self.video {
            StreamControl::CloseStream {
                stream: stream.to_string(),
                codec: Some("h264".to_string()),
                tier: self.selected_tier.clone(),
            }
        } else {
            StreamControl::CloseStream {
                stream: stream.to_string(),
                codec: Some("mjpeg".to_string()),
                tier: None,
            }
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
    /// previous tile for the stream). `video` records the profile
    /// (H.264 video vs JPEG preview) for the expand overlay (#436).
    pub fn open_tile(
        &mut self,
        stream: &str,
        generation: u64,
        abort: Option<iced::task::Handle>,
        video: bool,
        selected_tier: Option<String>,
    ) {
        self.tiles.insert(
            stream.to_string(),
            TileState::new(generation, abort, video, selected_tier),
        );
    }

    /// The tiers `stream` offers, per the catalogue (empty if unknown).
    pub fn offered_tiers(&self, stream: &str) -> &[TierSpec] {
        self.catalogue
            .ready()
            .and_then(|streams| streams.iter().find(|s| s.stream == stream))
            .map(|s| s.tiers.as_slice())
            .unwrap_or(&[])
    }

    /// Resolve a sensible DEFAULT tier for `stream` (used by the expand-upgrade
    /// path, which has no explicit tier — the per-tier buttons pass one): the
    /// `medium` tier if offered, else the highest-quality offered tier (the
    /// ladder's tail). `None` only when the catalogue lists no tiers.
    pub fn resolve_tier(&self, stream: &str) -> Option<String> {
        let tiers = self.offered_tiers(stream);
        if tiers.is_empty() {
            return None;
        }
        tiers
            .iter()
            .find(|t| t.name == "medium")
            .or_else(|| tiers.last())
            .map(|t| t.name.clone())
    }

    /// Expand `stream`'s tile into the near-fullscreen overlay (#436),
    /// recording its current profile so collapse can restore it. No-op for
    /// unknown tiles. Returns whether the tile still runs the preview
    /// profile (i.e. the caller should upgrade it to video).
    pub fn expand(&mut self, stream: &str) -> Option<bool> {
        let tile = self.tiles.get(stream)?;
        let was_video = tile.video;
        self.expanded = Some(ExpandedTile {
            stream: stream.to_string(),
            was_video,
        });
        Some(!was_video)
    }

    /// Dismiss the expanded-tile overlay, handing back what was expanded so
    /// the caller can restore the pre-expand profile.
    pub fn collapse(&mut self) -> Option<ExpandedTile> {
        self.expanded.take()
    }

    /// The expanded tile's state, if the overlay is up AND the tile still
    /// exists (a closed/torn-down tile dismisses the overlay implicitly).
    pub fn expanded_tile(&self) -> Option<(&str, &TileState)> {
        let expanded = self.expanded.as_ref()?;
        let tile = self.tiles.get(&expanded.stream)?;
        Some((expanded.stream.as_str(), tile))
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

    /// Fold a sensor-side `StreamStatus` transition (`state/parallax/stream/<stream>`) in:
    /// a definitive `open: false` for a tile still waiting for its first
    /// frame means the open failed on the sensor — surface it instead of
    /// "waiting for frames…" forever. The subscriber task stays alive (the
    /// hint self-heals if frames do arrive later: `apply_frame` clears it).
    pub fn apply_stream_status(&mut self, status: &StreamStatus) {
        // Keep the latest per-tier applied params + viewer counts for the
        // tile's bandwidth readout (#503).
        self.status.insert(status.stream.clone(), status.clone());
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

    /// The applied params the sensor reports for `stream`'s `tier`, if that
    /// tier is currently open (drives the honest per-tile bandwidth readout).
    pub fn applied_tier(
        &self,
        stream: &str,
        tier: &str,
    ) -> Option<&zensight_common::stream::TierStatus> {
        self.status
            .get(stream)?
            .tiers
            .iter()
            .find(|t| t.tier == tier)
    }

    /// Close one tile: abort its subscriber task and drop it. Dismisses the
    /// expanded overlay if it was showing this stream.
    pub fn close_tile(&mut self, stream: &str) {
        if self
            .expanded
            .as_ref()
            .is_some_and(|expanded| expanded.stream == stream)
        {
            self.expanded = None;
        }
        if let Some(tile) = self.tiles.remove(stream)
            && let Some(abort) = tile.abort
        {
            abort.abort();
        }
    }

    /// Tear every tile down (device deselected / disconnected): abort all
    /// subscriber tasks, clear the map (and the expanded overlay with it),
    /// and return each tile's `(stream, CloseStream)` so the caller can batch
    /// the profile-correct `close_stream` commands (a codec-less close would
    /// reap the wrong per-tier refcount).
    pub fn teardown(&mut self) -> Vec<(String, StreamControl)> {
        self.expanded = None;
        let closes: Vec<(String, StreamControl)> = self
            .tiles
            .iter()
            .map(|(stream, tile)| (stream.clone(), tile.close_control(stream)))
            .collect();
        for (_, tile) in std::mem::take(&mut self.tiles) {
            if let Some(abort) = tile.abort {
                abort.abort();
            }
        }
        closes
    }
}

/// Query the stream catalogue (the `streams` procedure): the host's concrete
/// @rpc key when its origin is known, else the fleet selector (first reply —
/// right on a single-host mesh, and the origin map fills within ~5 s).
pub async fn fetch_streams(
    session: Arc<Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<StreamDescriptor>> {
    let key = match origin {
        Some(o) => origin_rpc_key(&o, "parallax", "streams"),
        None => zensight_common::fleet_rpc_key("parallax", "streams"),
    };
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
    origin: zenkey::RemoteOrigin,
    stream: String,
    generation: u64,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        // The preview key for one stream on one host, exact on every chunk.
        // The origin is a parsed type rather than a string (#649): a `*` here
        // would fan the JPEG stream in from every host running a camera of
        // this name, which RFC 07 §3 forbids on the bulk planes. The caller
        // resolves the origin and toasts if it cannot (`app.rs`), so the
        // "before the map fills" window this used to cover no longer exists
        // (#474).
        let key = media_preview_key(&origin, &stream);
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
    /// A parsed origin for the drill-down key tests (#485): the builders take
    /// a `RemoteOrigin` now, so a test cannot hand them a string that would
    /// never have routed.
    fn test_origin() -> zenkey::RemoteOrigin {
        zenkey::RemoteOrigin::parse("h-3fa9c2d41b7e").expect("valid test origin")
    }

    use super::*;

    fn dummy_handle() -> image::Handle {
        image::Handle::from_rgba(2, 2, vec![0u8; 16])
    }

    #[test]
    fn preview_key_is_verbatim_media_plane() {
        // Pin the exact key the tile stream subscribes to — the sensor
        // publishes on this literal key (cross-crate contract, RFC 07 §1).
        assert_eq!(
            media_preview_key(&test_origin(), "cam0"),
            "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
        );
        assert_eq!(
            origin_rpc_key(&test_origin(), "parallax", "streams"),
            "v1/h-3fa9c2d41b7e/@rpc/parallax/streams"
        );
    }

    #[test]
    fn frames_replace_and_stale_sequences_drop() {
        let mut state = ParallaxDetailState::default();
        let generation = state.allocate_generation();
        state.open_tile("cam0", generation, None, false, None);

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
        state.open_tile("cam0", old, None, false, None);
        assert!(state.apply_frame("cam0", old, 500, dummy_handle()));

        // Replace the tile (e.g. preview → video switch): a leftover frame
        // from the old subscriber must not land on the new tile — its
        // sequence domain (500…) would freeze the new tile at seq 0….
        let new = state.allocate_generation();
        state.open_tile("cam0", new, None, false, None);
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
        state.open_tile("cam0", generation, None, false, None);
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
        state.open_tile("cam0", old, None, false, None);
        let new = state.allocate_generation();
        state.open_tile("cam0", new, None, false, None);

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
        state.open_tile("cam0", generation, None, false, None);

        // open: true transitions never touch the tile.
        state.apply_stream_status(&StreamStatus {
            stream: "cam0".into(),
            open: true,
            tiers: Vec::new(),
        });
        assert!(state.tiles["cam0"].ended.is_none());

        // A definitive open: false while still waiting = failed open.
        let closed = StreamStatus {
            stream: "cam0".into(),
            open: false,
            tiers: Vec::new(),
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
        state.open_tile("cam0", g0, None, false, None);
        let g1 = state.allocate_generation();
        state.open_tile("cam1", g1, None, false, None);
        assert!(state.is_open("cam0"));

        state.end_tile("cam0", g0, Some("boom".into()));
        assert_eq!(state.tiles["cam0"].ended.as_deref(), Some("boom"));

        state.close_tile("cam0");
        assert!(!state.is_open("cam0"));

        let torn = state.teardown();
        let torn_streams: Vec<String> = torn.into_iter().map(|(stream, _)| stream).collect();
        assert_eq!(torn_streams, vec!["cam1".to_string()]);
        assert!(state.tiles.is_empty());
    }

    #[test]
    fn expand_collapse_lifecycle() {
        let mut state = ParallaxDetailState::default();
        let generation = state.allocate_generation();
        state.open_tile("cam0", generation, None, false, None);

        // Expanding an unknown tile is a no-op.
        assert!(state.expand("nope").is_none());
        assert!(state.expanded.is_none());

        // Expanding a preview tile asks the caller to upgrade to video.
        assert_eq!(state.expand("cam0"), Some(true));
        assert_eq!(
            state.expanded_tile().map(|(name, _)| name),
            Some("cam0"),
            "overlay shows the expanded tile"
        );

        // The upgrade replaces the tile with a video incarnation; the
        // expansion (keyed by stream) survives and remembers the pre-expand
        // profile.
        let upgraded = state.allocate_generation();
        state.open_tile("cam0", upgraded, None, true, Some("high".to_string()));
        assert!(state.expanded_tile().is_some_and(|(_, tile)| tile.video));

        // Collapse hands the expansion back so the caller can restore.
        let expanded = state.collapse().expect("was expanded");
        assert_eq!(expanded.stream, "cam0");
        assert!(!expanded.was_video, "tile ran preview before expand");
        assert!(state.expanded.is_none());
        assert!(state.collapse().is_none(), "second collapse is a no-op");

        // A tile already on video expands without an upgrade request.
        assert_eq!(state.expand("cam0"), Some(false));
    }

    #[test]
    fn close_and_teardown_dismiss_the_expansion() {
        let mut state = ParallaxDetailState::default();
        let g0 = state.allocate_generation();
        state.open_tile("cam0", g0, None, true, Some("high".to_string()));
        let g1 = state.allocate_generation();
        state.open_tile("cam1", g1, None, false, None);

        // Closing an unrelated tile keeps the overlay up…
        state.expand("cam0");
        state.close_tile("cam1");
        assert!(state.expanded_tile().is_some());
        // …closing the expanded tile dismisses it.
        state.close_tile("cam0");
        assert!(state.expanded.is_none());

        // Teardown (device/view switch, disconnect) always dismisses.
        let g2 = state.allocate_generation();
        state.open_tile("cam0", g2, None, false, None);
        state.expand("cam0");
        state.teardown();
        assert!(state.expanded.is_none());
        assert!(state.expanded_tile().is_none());
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
            width: Some(640),
            height: Some(480),
            fps: Some(30.0),
            tiers: vec![TierSpec {
                name: "high".into(),
                max_height: None,
                fps: 30,
                bitrate_kbps: 4000,
            }],
            description: None,
        }]));
        assert_eq!(state.catalogue.ready().map(|v| v.len()), Some(1));
        state.apply(Err("no sensor".into()));
        assert_eq!(state.catalogue.error(), Some("no sensor"));
    }

    fn spec(name: &str, max_height: Option<u32>) -> TierSpec {
        TierSpec {
            name: name.into(),
            max_height,
            fps: 30,
            bitrate_kbps: 4000,
        }
    }

    fn ladder_descriptor() -> StreamDescriptor {
        StreamDescriptor {
            stream: "cam0".into(),
            codecs: vec!["h264".into(), "mjpeg".into()],
            active: false,
            width: Some(1280),
            height: Some(720),
            fps: Some(30.0),
            tiers: vec![
                spec("low", Some(240)),
                spec("medium", Some(480)),
                spec("high", None),
            ],
            description: None,
        }
    }

    #[test]
    fn close_control_names_the_tiles_own_profile() {
        // A preview closes on mjpeg with no tier; a video tile closes on its
        // EXACT tier — never a codec-less close (which the sensor would resolve
        // to its default video tier, decrementing the wrong refcount).
        let preview = TileState::new(1, None, false, None);
        assert!(matches!(
            preview.close_control("cam0"),
            StreamControl::CloseStream { codec: Some(c), tier: None, .. } if c == "mjpeg"
        ));
        let video = TileState::new(2, None, true, Some("low".into()));
        assert!(matches!(
            video.close_control("cam0"),
            StreamControl::CloseStream { codec: Some(c), tier: Some(t), .. }
                if c == "h264" && t == "low"
        ));
    }

    #[test]
    fn resolve_tier_picks_medium_then_the_ladder_tail() {
        let mut state = ParallaxDetailState::default();
        // No catalogue yet → nothing to resolve.
        assert_eq!(state.resolve_tier("cam0"), None);

        state.apply(Ok(vec![ladder_descriptor()]));
        // The default-tier resolver (expand-upgrade path) prefers `medium` when
        // offered, else the highest tier the camera can feed.
        assert_eq!(state.resolve_tier("cam0").as_deref(), Some("medium"));
    }
}
