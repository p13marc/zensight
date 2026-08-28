//! The stream-session actor: single owner of all mutable stream state.
//!
//! One task owns the `stream → StreamSession` map (no shared locks); the
//! command loop, streams queryable, matching listeners, and egress tasks all
//! talk to it through a bounded [`SessionHandle`] mpsc. Deadlock-freedom:
//! producers only send into the channel, the actor never awaits its own
//! queue, oneshot replies are fire-and-forget, and pipeline `abort()` is
//! synchronous.
//!
//! Each stream can hold up to two independently refcounted profile pipelines
//! (video = H.264, preview = JPEG) — see `docs/streams.md`. Teardown policy:
//! a profile with **no matching viewers** enters an idle countdown (started
//! at open, on a viewers falling edge, or on the last close) and is reaped
//! after `idle_timeout_secs`; explicit refcounts keep it open only while a
//! viewer is still expected. The viewers-based countdown also reaps profiles
//! whose opener died without `close_stream` (crash backstop).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parallax::elements::{
    MediaType as RtspMediaType, RtspReconnect, RtspSession, RtspSrc, RtspStreamInfoHandle,
};
use parallax::pipeline::UnifiedPipelineHandle as PipelineHandle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zenoh::bytes::Encoding;
use zensight_common::QosClass;
use zensight_common::stream::{StreamControl, StreamEnd, StreamEndReason, StreamStatus};
use zensight_sensor_core::{Publisher, RawMediaPublisher, SensorHealth};

use crate::alerts::ParallaxAlerts;
use crate::catalog::{Catalog, SourceKind};
use crate::config::ParallaxConfig;
use crate::stats::{StatsRegistry, StreamStats};
use crate::{egress, pipeline};

/// Capacity of the actor's command channel.
const CHANNEL_CAPACITY: usize = 64;

/// When switching video tiers on an exclusive source (V4L2/RTSP), the just
/// released sibling capture frees its device asynchronously — its source loop
/// runs on a blocking thread that only exits on its next iteration. Retry the
/// new tier's build this many times, spaced by [`BUILD_RETRY_DELAY`], to absorb
/// that hand-over window (`EBUSY`) before giving up (~1s total).
const BUILD_RETRY_MAX: u32 = 20;
const BUILD_RETRY_DELAY: Duration = Duration::from_millis(50);

/// One per-stream media profile. A stream has at most one `Preview` and one
/// `Video(tier)` per tier index — distinct tiers are **independent** encoders
/// published concurrently on distinct `@media/<stream>/video/h264/<tier>` keys,
/// so two viewers on different links never fight over one encoder (#494). The
/// `u8` indexes the config tier ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// `@media/<stream>/video/h264/<tier>` — one bandwidth tier.
    Video(u8),
    /// `@media/<stream>/preview/jpeg` — low-fps JPEG previews.
    Preview,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Profile::Video(_) => "video",
            Profile::Preview => "preview",
        }
    }

    /// The tier index for a video profile (`None` for the preview).
    fn tier_index(self) -> Option<u8> {
        match self {
            Profile::Video(idx) => Some(idx),
            Profile::Preview => None,
        }
    }

    /// The `{tier}` key chunk for this profile: the ladder rung's name, or the
    /// literal `preview`.
    ///
    /// Used by the receiver-feedback aggregate (#715), which is per tier.
    pub fn tier_label(self, tier_names: &[&str]) -> Option<String> {
        match self {
            Profile::Video(idx) => tier_names.get(idx as usize).map(|n| (*n).to_string()),
            Profile::Preview => Some("preview".to_string()),
        }
    }
}

/// Resolve a `(codec, tier)` selector against a tier ladder, without a session.
///
/// The **one** place that mapping lives, so a `MediaReceiverReport` can never
/// name a key an `OpenStream` could not (#715). `ProfileSessionActor::resolve_profile`
/// is a thin wrapper over it; `crate::reports` calls it directly, because the
/// report path deliberately holds no handle to the session actor — see that
/// module's header for why that boundary is structural rather than stylistic.
pub fn resolve_profile_in(
    codec: Option<&str>,
    tier: Option<&str>,
    tier_names: &[&str],
    default_tier: &str,
) -> Option<Profile> {
    let index_of = |name: &str| tier_names.iter().position(|n| *n == name).map(|i| i as u8);
    match codec {
        None | Some("h264") => {
            let idx = match tier {
                Some(name) => index_of(name)?,
                // The configured default is validated to exist; 0 is the last
                // resort, matching the actor's own fallback.
                None => index_of(default_tier).unwrap_or(0),
            };
            Some(Profile::Video(idx))
        }
        Some("mjpeg") | Some("jpeg") => Some(Profile::Preview),
        Some(_) => None,
    }
}

/// Messages into the session actor.
pub enum SessionMsg {
    /// A decoded stream-control command.
    Control(StreamControl),
    /// A profile's matching listener saw a viewer appear/disappear.
    ViewersChanged {
        stream: String,
        profile: Profile,
        matching: bool,
    },
    /// A profile's egress loop ended (pipeline EOS or error). `epoch`
    /// identifies WHICH incarnation of the profile ended: a dead profile
    /// that `open()` already tore down and replaced leaves its `EgressEnded`
    /// queued, and acting on the stale one would kill the replacement.
    EgressEnded {
        stream: String,
        profile: Profile,
        epoch: u64,
        /// How it ended — the sink's own account, not a flattened
        /// `Option<String>` (#691).
        end: crate::egress::EgressEnd,
    },
    /// An off-actor RTSP connect finished; `Ok` carries the live camera
    /// session for the pending profile slot.
    RtspConnected {
        stream: String,
        profile: Profile,
        result: Result<Box<RtspSession>, String>,
    },
    /// Snapshot request: one `StreamStatus` per currently open stream.
    StatusQuery {
        reply: oneshot::Sender<Vec<StreamStatus>>,
    },
}

/// Cheap cloneable sender into the session actor.
#[derive(Clone)]
pub struct SessionHandle(mpsc::Sender<SessionMsg>);

impl SessionHandle {
    /// Send a message; drops it (with a log) if the actor died.
    pub async fn send(&self, msg: SessionMsg) {
        if self.0.send(msg).await.is_err() {
            tracing::warn!("session actor is gone; dropping message");
        }
    }

    /// Snapshot the open-stream statuses (empty if the actor died).
    pub async fn statuses(&self) -> Vec<StreamStatus> {
        let (tx, rx) = oneshot::channel();
        if self
            .0
            .send(SessionMsg::StatusQuery { reply: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// The set of stream names with at least one open profile.
    pub async fn open_streams(&self) -> HashSet<String> {
        self.statuses()
            .await
            .into_iter()
            .filter(|s| s.open)
            .map(|s| s.stream)
            .collect()
    }
}

/// One profile slot: either an off-actor RTSP connect still in flight, or a
/// running pipeline.
enum ProfileSlot {
    /// The RTSP connect runs in its own task (never inside the actor — an
    /// unreachable camera would stall every stream command and status /
    /// catalogue reply for up to the connect timeout). The reservation
    /// refcounts opens/closes that race the connect; `RtspConnected`
    /// resolves it into `Open` or clears it.
    Pending(PendingOpen),
    Open(Box<ProfileSession>),
}

/// Bookkeeping for a profile whose RTSP connect is still in flight.
struct PendingOpen {
    /// Explicit `open_stream` minus `close_stream` received while pending;
    /// 0 by the time the connect resolves = every opener left, so the
    /// camera session is discarded instead of installed.
    refcount: u32,
}

/// One running profile pipeline + its egress/matcher tasks.
struct ProfileSession {
    handle: Option<PipelineHandle>,
    /// Handles cloned from this profile's elements before the executor moved
    /// them into their tasks. Only `keyframe` is driven at runtime; the rest
    /// are observation (`encoder_stats`, #510) or an unreached retune path —
    /// see [`pipeline::PipelineControls`].
    controls: pipeline::PipelineControls,
    egress: JoinHandle<()>,
    matcher: JoinHandle<()>,
    /// Kept so the declared media publisher lives exactly as long as the
    /// profile (undeclared on drop).
    #[allow(dead_code)]
    publisher: Arc<RawMediaPublisher>,
    /// Explicit `open_stream` minus `close_stream` count.
    refcount: u32,
    /// Incarnation number (monotonic across the actor): stamps this
    /// profile's `EgressEnded` so a stale end report from a torn-down
    /// predecessor cannot kill its replacement.
    epoch: u64,
    /// Whether the media publisher currently has matching subscribers.
    viewers: bool,
    /// Set while unwatched (no viewers); reaped after `idle_timeout`.
    idle_since: Option<Instant>,
    /// Encoded dimensions this profile was built at (the scaler's target), for
    /// the per-tier status doc. `0` = unknown (RTSP passthrough without SDP).
    width: u32,
    height: u32,
    /// Last `frames_dropped_by_rc` folded from this incarnation's encoder
    /// handle — the baseline [`StreamStats::fold_rc_drops`] subtracts against.
    rc_drops_seen: u64,
    /// Pull side of this profile's terminal `AppSink`, kept for its live
    /// counters (`total_dropped`, `queued_buffers`) — the element itself is
    /// inside its executor task and unreachable, which is exactly why
    /// upstream puts `stats()` on the handle (#692).
    sink: parallax::elements::AppSinkHandle,
    /// Last `total_dropped` folded from this incarnation's sink — the baseline
    /// [`StreamStats::fold_sink_drops`] subtracts against.
    sink_drops_seen: u64,
}

impl Drop for ProfileSession {
    fn drop(&mut self) {
        // Belt-and-braces: however this session dies (explicit teardown,
        // actor shutdown, panic unwind, runtime teardown), ask the sources to
        // stop. A live source that nobody asked keeps its task — and its
        // device — alive, and tokio's shutdown would wait on it forever.
        // `stop` borrows, which is the whole reason `Drop` can call it.
        if let Some(handle) = &self.handle {
            handle.stop();
        }
    }
}

impl ProfileSession {
    fn teardown(mut self) {
        // Ask before cutting (#709). `stop()` raises the executor's
        // cooperative flag, which every source loop checks at the top of its
        // next iteration: the loop ends, EOS travels downstream, and the
        // source drops its device on the way out. `abort()` also raises that
        // flag, but it cancels the tasks in the same breath, so a source
        // holding an exclusive V4L2 device may still be holding it when the
        // replacement tier tries to open — which is `EBUSY`, and which is why
        // `release_conflicting_video_tiers` exists at all.
        if let Some(handle) = &self.handle {
            handle.stop();
        }
        self.egress.abort();
        self.matcher.abort();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// A stream's open profile slots, keyed by [`Profile`] — one `Preview` plus any
/// number of `Video(tier)` slots (each an independent tier pipeline).
#[derive(Default)]
struct StreamSession {
    slots: HashMap<Profile, ProfileSlot>,
}

impl StreamSession {
    fn slot_ref(&self, profile: Profile) -> Option<&ProfileSlot> {
        self.slots.get(&profile)
    }

    fn slot_mut(&mut self, profile: Profile) -> Option<&mut ProfileSlot> {
        self.slots.get_mut(&profile)
    }

    fn insert_slot(&mut self, profile: Profile, slot: ProfileSlot) {
        self.slots.insert(profile, slot);
    }

    fn remove_slot(&mut self, profile: Profile) -> Option<ProfileSlot> {
        self.slots.remove(&profile)
    }

    /// The occupied profile slots (unordered).
    fn profiles(&self) -> impl Iterator<Item = (Profile, &ProfileSlot)> {
        self.slots.iter().map(|(p, s)| (*p, s))
    }

    /// The occupied profile slots, mutably (unordered).
    fn profiles_mut(&mut self) -> impl Iterator<Item = &mut ProfileSlot> {
        self.slots.values_mut()
    }

    /// Open profiles with matching subscribers (pending slots have none).
    fn viewers(&self) -> u32 {
        self.profiles()
            .filter(|(_, slot)| matches!(slot, ProfileSlot::Open(p) if p.viewers))
            .count() as u32
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// The actor: owns the session map, spawned once at sensor startup.
pub struct SessionManager {
    catalog: Arc<Catalog>,
    config: ParallaxConfig,
    /// Legacy instance label; keys no longer carry it (v1 origin does, epic
    /// #453) but stream status payloads may still reference it.
    #[allow(dead_code)]
    source: String,
    publisher: Publisher,
    /// v1: per-stream LWW status docs (`state/parallax/stream/<stream>`,
    /// RFC 05 §5) — the catalogue+status document; tombstoned on removal
    /// from config, never on close.
    state_ctx: zensight_sensor_core::v1::V1Context,
    sessions: HashMap<String, StreamSession>,
    tx: mpsc::Sender<SessionMsg>,
    /// Per-stream stats counters (fed by egress/encoders, read by the ticker).
    stats: StatsRegistry,
    /// Per-stream health (device liveness / consecutive-failure tracking).
    health: Option<Arc<SensorHealth>>,
    /// Alert rules (rtsp_connect_failed fires from the open path).
    alerts: Option<Arc<ParallaxAlerts>>,
    /// Next `ProfileSession::epoch` (monotonic across all profiles).
    next_epoch: u64,
    /// Why each stream's most recent tier stopped, kept **alive across the
    /// teardown that removed the slot** (#691).
    ///
    /// `teardown_profile` deletes the `StreamSession` before `publish_status`
    /// runs, and `status_for` reads nothing but `self.sessions` — so without
    /// this the actor destroys the evidence one line before it publishes, and
    /// `publish_status`'s no-session arm can see nothing at all.
    ///
    /// Keyed by stream, which bounds it to the catalogue: one entry per
    /// configured stream, overwritten by each newer end, cleared when that
    /// tier streams again.
    last_end: HashMap<String, StreamEnd>,
}

impl SessionManager {
    /// Spawn the actor task and return its handle.
    pub fn spawn(
        catalog: Arc<Catalog>,
        config: ParallaxConfig,
        source: String,
        publisher: Publisher,
        stats: StatsRegistry,
        health: Option<Arc<SensorHealth>>,
        alerts: Option<Arc<ParallaxAlerts>>,
    ) -> SessionHandle {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let state_ctx = zensight_sensor_core::v1::for_producer("parallax");
        let manager = SessionManager {
            catalog,
            config,
            source,
            publisher,
            state_ctx,
            sessions: HashMap::new(),
            tx: tx.clone(),
            stats,
            health,
            alerts,
            next_epoch: 0,
            last_end: HashMap::new(),
        };
        tokio::spawn(manager.run(rx));
        SessionHandle(tx)
    }

    async fn run(mut self, mut rx: mpsc::Receiver<SessionMsg>) {
        let mut reap_tick = tokio::time::interval(Duration::from_secs(1));
        reap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.handle(msg).await,
                    None => break,
                },
                _ = reap_tick.tick() => {
                    self.fold_pipeline_stats();
                    self.reap_idle().await;
                }
            }
        }
        // Actor shutdown: tear every remaining profile down (pending
        // reservations have nothing running; dropping them is enough).
        for (_, mut session) in self.sessions.drain() {
            for (_, slot) in session.slots.drain() {
                if let ProfileSlot::Open(p) = slot {
                    p.teardown();
                }
            }
        }
        tracing::info!("session actor stopped");
    }

    async fn handle(&mut self, msg: SessionMsg) {
        match msg {
            SessionMsg::Control(control) => self.handle_control(control).await,
            SessionMsg::ViewersChanged {
                stream,
                profile,
                matching,
            } => self.handle_viewers(&stream, profile, matching).await,
            SessionMsg::EgressEnded {
                stream,
                profile,
                epoch,
                end,
            } => self.handle_egress_ended(&stream, profile, epoch, end).await,
            SessionMsg::RtspConnected {
                stream,
                profile,
                result,
            } => self.handle_rtsp_connected(&stream, profile, result).await,
            SessionMsg::StatusQuery { reply } => {
                let _ = reply.send(self.statuses());
            }
        }
    }

    /// Resolve an `(codec, tier)` selector to a [`Profile`]. `None`/`h264`
    /// codec → a video tier (named, or the sensor default); `mjpeg`/`jpeg` →
    /// the preview. An unknown codec or an unknown tier name → `None`.
    fn resolve_profile(&self, codec: Option<&str>, tier: Option<&str>) -> Option<Profile> {
        let names: Vec<&str> = self
            .config
            .video
            .tiers
            .iter()
            .map(|t| t.spec.name.as_str())
            .collect();
        resolve_profile_in(codec, tier, &names, &self.config.video.default_tier)
    }

    /// The config ladder index of a tier by name.
    fn tier_index(&self, name: &str) -> Option<u8> {
        self.config
            .video
            .tiers
            .iter()
            .position(|t| t.spec.name == name)
            .map(|i| i as u8)
    }

    /// The default tier's ladder index (validated to exist; 0 as a last resort).
    fn default_tier_index(&self) -> u8 {
        self.tier_index(&self.config.video.default_tier)
            .unwrap_or(0)
    }

    /// The ladder rung at an index — the wire spec plus its sensor-local
    /// encoder shaping (#509).
    fn tier_config(&self, idx: u8) -> Option<&crate::config::TierConfig> {
        self.config.video.tiers.get(idx as usize)
    }

    /// The tier name for a ladder index (for the `<tier>` media key chunk).
    fn tier_name(&self, idx: u8) -> &str {
        self.tier_config(idx)
            .map(|t| t.spec.name.as_str())
            .unwrap_or("default")
    }

    /// Resolve a tier index to the encoder parameters `build_video` needs:
    /// the rung's advertised numbers plus its shaping resolved over the
    /// shared `video.encoder` defaults.
    fn tier_params(&self, idx: u8) -> pipeline::VideoParams {
        let video = &self.config.video;
        let (bitrate_kbps, fps, max_height, tuning) = self
            .tier_config(idx)
            .map(|t| {
                (
                    t.spec.bitrate_kbps,
                    t.spec.fps,
                    t.spec.max_height,
                    video.tuning_for(t),
                )
            })
            .unwrap_or((2000, 30, None, video.encoder));
        pipeline::VideoParams {
            bitrate_kbps,
            gop_frames: tuning.gop_frames.unwrap_or(video.gop_frames),
            fps,
            max_height,
            tuning,
        }
    }

    async fn handle_control(&mut self, control: StreamControl) {
        match control {
            StreamControl::OpenStream {
                stream,
                codec,
                tier,
            } => {
                let Some(profile) = self.resolve_profile(codec.as_deref(), tier.as_deref()) else {
                    tracing::warn!(stream = %stream, codec = ?codec, tier = ?tier,
                        "open_stream: unsupported codec or unknown tier");
                    return;
                };
                self.open(&stream, profile).await;
            }
            StreamControl::CloseStream {
                stream,
                codec,
                tier,
            } => {
                let Some(profile) = self.resolve_profile(codec.as_deref(), tier.as_deref()) else {
                    tracing::warn!(stream = %stream, codec = ?codec, tier = ?tier,
                        "close_stream: unsupported codec or unknown tier");
                    return;
                };
                self.close(&stream, profile).await;
            }
            StreamControl::RequestKeyframe { stream, tier } => {
                let idx = match tier.as_deref() {
                    Some(name) => match self.tier_index(name) {
                        Some(i) => i,
                        None => {
                            tracing::warn!(stream = %stream, tier = ?tier,
                                "request_keyframe: unknown tier");
                            return;
                        }
                    },
                    None => self.default_tier_index(),
                };
                self.request_keyframe(&stream, Profile::Video(idx));
            }
        }
    }

    async fn open(&mut self, stream: &str, profile: Profile) {
        let Some(entry) = self.catalog.get(stream) else {
            tracing::warn!(stream = %stream, "open_stream: unknown stream");
            return;
        };
        let kind = entry.kind.clone();

        // Reuse (or clear) an existing slot for the profile.
        enum Existing {
            None,
            Reused,
            Dead,
        }
        let existing = match self
            .sessions
            .get_mut(stream)
            .and_then(|s| s.slot_mut(profile))
        {
            None => Existing::None,
            Some(ProfileSlot::Pending(pending)) => {
                // An RTSP connect is already in flight: count the opener in.
                pending.refcount = pending.refcount.saturating_add(1);
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    refcount = pending.refcount, "open_stream: open already in flight");
                Existing::Reused
            }
            Some(ProfileSlot::Open(existing)) if existing.egress.is_finished() => {
                // The egress already ended but its EgressEnded is still
                // queued behind this OpenStream: the pipeline is dead and
                // would never feed this opener. Tear it down and rebuild.
                Existing::Dead
            }
            Some(ProfileSlot::Open(existing)) => {
                // Healthy and open: bump the refcount, refresh the idle
                // countdown, and hand the (re)opener a fresh IDR.
                existing.refcount = existing.refcount.saturating_add(1);
                if !existing.viewers {
                    existing.idle_since = Some(Instant::now());
                }
                if let Some(k) = &existing.controls.keyframe {
                    k.request();
                }
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    refcount = existing.refcount, "open_stream: profile already open");
                Existing::Reused
            }
        };
        match existing {
            Existing::Reused => {
                self.publish_status(stream).await;
                return;
            }
            Existing::Dead => {
                tracing::info!(stream = %stream, profile = profile.as_str(),
                    "open_stream: replacing a dead profile (egress already ended)");
                // The corpse is being replaced, not stopped. `finish_open`
                // clears this entry moments later when the replacement is
                // installed on the same tier, so it is only ever visible if
                // the rebuild itself then fails — in which case `FailedOpen`
                // overwrites it with the truth.
                self.teardown_profile(stream, profile, StreamEndReason::Superseded);
            }
            Existing::None => {}
        }

        // A single camera can't feed two captures at once: on an exclusive
        // source (V4L2/RTSP) opening a video tier must first release any other
        // open video tier for this stream, or the new capture fails `EBUSY`
        // while the old tier lingers in its idle window. No-op for the
        // shareable test source, so concurrent tiers (#494) still work there.
        if matches!(profile, Profile::Video(_)) && kind.is_exclusive() {
            self.release_conflicting_video_tiers(stream, profile).await;
        }

        // Build the profile pipeline. Test/V4L2 sources are self-driving and
        // built synchronously; RTSP must connect first, which is NEVER done
        // inside the actor (an unreachable camera would stall every stream
        // command and status/catalogue reply for the connect timeout): the
        // slot is reserved as Pending and `RtspConnected` resolves it.
        let stream_stats = self.stats.handle(stream);
        match &kind {
            SourceKind::Rtsp {
                url,
                username,
                password,
            } => {
                self.sessions
                    .entry(stream.to_string())
                    .or_default()
                    .insert_slot(profile, ProfileSlot::Pending(PendingOpen { refcount: 1 }));
                let tx = self.tx.clone();
                let (url, username, password) = (url.clone(), username.clone(), password.clone());
                let task_stream = stream.to_string();
                tokio::spawn(async move {
                    let result = connect_rtsp(&url, username.as_deref(), password.as_deref())
                        .await
                        .map(Box::new)
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(SessionMsg::RtspConnected {
                            stream: task_stream,
                            profile,
                            result,
                        })
                        .await;
                });
                self.publish_status(stream).await;
            }
            _ => {
                let built = match profile {
                    Profile::Video(idx) => {
                        let mut built =
                            pipeline::build_video(&kind, &self.tier_params(idx), &stream_stats);
                        // Absorb the just-released sibling's async device
                        // hand-over (see BUILD_RETRY_MAX): retry the exclusive
                        // capture a few times before surfacing the failure.
                        if kind.is_exclusive() {
                            let mut tries = 0;
                            while built.is_err() && tries < BUILD_RETRY_MAX {
                                tries += 1;
                                tokio::time::sleep(BUILD_RETRY_DELAY).await;
                                built = pipeline::build_video(
                                    &kind,
                                    &self.tier_params(idx),
                                    &stream_stats,
                                );
                            }
                        }
                        built
                    }
                    Profile::Preview => {
                        pipeline::build_preview(&kind, &self.config.preview, &stream_stats)
                    }
                };
                self.finish_open(stream, profile, built, stream_stats, 1)
                    .await;
            }
        }
    }

    /// An off-actor RTSP connect resolved: install the profile (or clean the
    /// pending reservation up).
    async fn handle_rtsp_connected(
        &mut self,
        stream: &str,
        profile: Profile,
        result: Result<Box<RtspSession>, String>,
    ) {
        let refcount = match self.sessions.get(stream).and_then(|s| s.slot_ref(profile)) {
            Some(ProfileSlot::Pending(p)) => p.refcount,
            _ => {
                // The reservation is gone (torn down meanwhile): dropping an
                // Ok result hangs the camera session up, nothing else to do.
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    "rtsp connect resolved for a slot that is no longer pending; dropped");
                return;
            }
        };
        if let Some(alerts) = &self.alerts {
            alerts
                .rtsp_connect(stream, result.as_ref().err().map(String::as_str))
                .await;
        }
        let rtsp = match result {
            Ok(rtsp) => rtsp,
            Err(e) => {
                self.fail_open(stream, profile, &e).await;
                return;
            }
        };
        if refcount == 0 {
            // Every opener closed while we were connecting: don't start a
            // pipeline nobody wants (dropping the RtspSession hangs up).
            tracing::debug!(stream = %stream, profile = profile.as_str(),
                "rtsp connected but every opener closed meanwhile; discarding");
            self.clear_pending_slot(stream, profile);
            self.remove_stats_if_closed(stream);
            self.publish_status(stream).await;
            return;
        }
        // `add_async_source` MOVES the session into the graph, so anything we
        // need to *observe* about it must be taken first — the SDP geometry
        // below reads through this handle, which outlives the move (#731).
        let info = rtsp.stream_info_handle();
        let dims = rtsp_video_dimensions(&info);
        let stream_stats = self.stats.handle(stream);
        let rtsp = *rtsp;
        let built = match profile {
            // RTSP is passthrough — no encoder in the graph, so it offers a
            // single tier regardless of which tier was requested (documented).
            Profile::Video(_) => pipeline::build_rtsp_video_passthrough(rtsp, dims),
            Profile::Preview => match dims {
                Some((w, h)) => {
                    pipeline::build_rtsp_preview(rtsp, w, h, &self.config.preview, &stream_stats)
                }
                None => Err(anyhow::anyhow!(
                    "rtsp stream advertises no dimensions; preview needs the SDP size"
                )),
            },
        };
        self.finish_open(stream, profile, built, stream_stats, refcount)
            .await;
    }

    /// Complete an open with a constructed pipeline: declare the media
    /// publisher and matching listener, start the pipeline, spawn the egress
    /// (and RTSP feeder), install the `ProfileSession`, and publish the
    /// status transition. EVERY failure funnels through [`Self::fail_open`]
    /// — a late failure that skipped cleanup used to leak the stream's stats
    /// entry (phantom zero-valued stats telemetry forever).
    async fn finish_open(
        &mut self,
        stream: &str,
        profile: Profile,
        built: anyhow::Result<pipeline::BuiltPipeline>,
        stream_stats: Arc<StreamStats>,
        refcount: u32,
    ) {
        let mut built = match built {
            Ok(b) => b,
            Err(e) => {
                self.fail_open(stream, profile, &format!("failed to build pipeline: {e}"))
                    .await;
                return;
            }
        };

        // Sticky, and set here rather than at every fold: it is what makes
        // `rc_drops` reportable at all. A stream that only ever ran an RTSP
        // passthrough or a preview has no rate control, and a `0` there would
        // read as "the cap is not biting" (#510).
        if built.controls.encoder_stats.is_some() {
            stream_stats.track_rc();
        }

        // Declare the media publisher on the profile's concrete key — built
        // with the generated registry `Media` builders, so what this sensor
        // publishes and what the viewer subscribes to agree with the
        // `[[media]]` declarations in parallax.toml by construction.
        let key: String = {
            use zensight_common::registry::parallax::{Media, media_key};
            let m = match profile {
                Profile::Video(idx) => Media::video(stream, "h264", self.tier_name(idx)),
                Profile::Preview => Media::preview_jpeg(stream),
            };
            media_key(&zensight_common::PROFILE.local_origin(), &m).into()
        };
        let media = match self.publisher.raw_media_publisher(key.clone()).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                self.fail_open(
                    stream,
                    profile,
                    &format!("failed to declare media publisher on {key}: {e}"),
                )
                .await;
                return;
            }
        };

        // Matching listener → rising/falling viewer edges into the actor.
        let matcher = {
            let listener = match media.matching_listener().await {
                Ok(l) => l,
                Err(e) => {
                    self.fail_open(
                        stream,
                        profile,
                        &format!("failed to declare matching listener: {e}"),
                    )
                    .await;
                    return;
                }
            };
            let tx = self.tx.clone();
            let stream = stream.to_string();
            tokio::spawn(async move {
                while let Ok(status) = listener.recv_async().await {
                    let _ = tx
                        .send(SessionMsg::ViewersChanged {
                            stream: stream.clone(),
                            profile,
                            matching: status.matching(),
                        })
                        .await;
                }
            })
        };

        // Start the pipeline.
        let handle = match pipeline::executor().start(&mut built.pipeline) {
            Ok(h) => h,
            Err(e) => {
                matcher.abort();
                self.fail_open(stream, profile, &format!("failed to start pipeline: {e}"))
                    .await;
                return;
            }
        };

        // Egress task: pump the sink into the publisher, report the end
        // (stamped with this incarnation's epoch — see `EgressEnded`).
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let egress = {
            let sink = built.sink.clone();
            let media = media.clone();
            let tx = self.tx.clone();
            let stream = stream.to_string();
            let (width, height) = (built.width, built.height);
            let (encoding, preview) = match profile {
                Profile::Video(_) => (Encoding::VIDEO_H264, false),
                Profile::Preview => (Encoding::IMAGE_JPEG, true),
            };
            let egress_stats = stream_stats.clone();
            tokio::spawn(async move {
                let end =
                    egress::run(sink, media, encoding, width, height, preview, egress_stats).await;
                let _ = tx
                    .send(SessionMsg::EgressEnded {
                        stream,
                        profile,
                        epoch,
                        end,
                    })
                    .await;
            })
        };

        // First IDR right away so an already-waiting viewer decodes at once.
        if let Some(k) = &built.controls.keyframe {
            k.request();
        }

        // Seed the viewer state: a subscriber discovered between the
        // publisher and listener declarations would otherwise never produce
        // a rising edge (and the idle reaper would kill a watched stream).
        let viewers = media.has_viewers().await.unwrap_or(false);

        if let Some(health) = &self.health {
            health.record_device_success(stream);
        }
        tracing::info!(stream = %stream, profile = profile.as_str(), key = %key, viewers,
            "stream profile opened");
        self.sessions
            .entry(stream.to_string())
            .or_default()
            .insert_slot(
                profile,
                ProfileSlot::Open(Box::new(ProfileSession {
                    handle: Some(handle),
                    controls: built.controls,
                    egress,
                    matcher,
                    publisher: media,
                    refcount,
                    epoch,
                    viewers,
                    // Unwatched until a viewer actually subscribes: give the
                    // opener one idle window to show up, then reap (zombie-open
                    // backstop).
                    idle_since: if viewers { None } else { Some(Instant::now()) },
                    width: built.width,
                    height: built.height,
                    rc_drops_seen: 0,
                    sink: built.sink,
                    sink_drops_seen: 0,
                })),
            );
        // A tier that is streaming again has no current end — but a *sibling's*
        // does, and this open must not erase it. Preview healthy, `high`
        // failed, operator opens `low`: the `high` failure is still the truth
        // about `high` and stays until `high` itself comes back (#691).
        let opened = profile.tier_label(&self.tier_names());
        if self
            .last_end
            .get(stream)
            .is_some_and(|e| Some(&e.tier) == opened.as_ref())
        {
            self.last_end.remove(stream);
        }
        self.publish_status(stream).await;
    }

    /// Single failure exit for every open path: log, record the device
    /// failure, drop any pending slot reservation, drop the stream's stats
    /// entry when nothing else is open (a leaked entry publishes phantom
    /// zero stats forever), and publish the `open: false` transition so a
    /// waiting viewer learns the open died (the GUI surfaces it on the tile).
    async fn fail_open(&mut self, stream: &str, profile: Profile, error: &str) {
        tracing::warn!(stream = %stream, profile = profile.as_str(), error = %error,
            "open_stream failed");
        if let Some(health) = &self.health {
            health.record_device_failure(stream, error);
        }
        // Distinct from a mid-stream `Failed` because this tier never ran, and
        // the two send an operator to different places: *it never started*
        // means check the config and whether the camera is reachable; *it
        // stopped* means check the element that failed. Nothing here is
        // `Open`, so this records the end directly rather than through
        // `teardown_profile`.
        self.note_end(
            stream,
            profile,
            StreamEndReason::FailedOpen {
                message: error.to_string(),
            },
        );
        self.clear_pending_slot(stream, profile);
        self.remove_stats_if_closed(stream);
        self.publish_status(stream).await;
    }

    /// Remove a pending reservation (never an `Open` slot — those must go
    /// through [`Self::teardown_profile`] so the pipeline's stop switch
    /// flips), dropping the stream entry when both slots are empty.
    fn clear_pending_slot(&mut self, stream: &str, profile: Profile) {
        if let Some(session) = self.sessions.get_mut(stream) {
            if matches!(session.slot_ref(profile), Some(ProfileSlot::Pending(_))) {
                session.remove_slot(profile);
            }
            if session.is_empty() {
                self.sessions.remove(stream);
            }
        }
    }

    /// `close_stream` decrements the refcount of the one profile it names
    /// (codec + tier — symmetric with the `open_stream` that raised it). A
    /// tier reaches zero independently of its siblings.
    async fn close(&mut self, stream: &str, profile: Profile) {
        let Some(session) = self.sessions.get_mut(stream) else {
            tracing::debug!(stream = %stream, "close_stream: not open");
            return;
        };
        match session.slot_mut(profile) {
            Some(ProfileSlot::Pending(p)) => {
                // Still connecting: `RtspConnected` discards the camera session
                // if the refcount is 0 by the time it resolves.
                p.refcount = p.refcount.saturating_sub(1);
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    refcount = p.refcount, "close_stream: pending refcount decremented");
            }
            Some(ProfileSlot::Open(p)) => {
                p.refcount = p.refcount.saturating_sub(1);
                if p.refcount == 0 && !p.viewers && p.idle_since.is_none() {
                    p.idle_since = Some(Instant::now());
                }
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    refcount = p.refcount, "close_stream: refcount decremented");
            }
            None => {
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    "close_stream: that profile is not open");
            }
        }
        self.publish_status(stream).await;
    }

    fn request_keyframe(&mut self, stream: &str, profile: Profile) {
        let keyframe = self
            .sessions
            .get(stream)
            .and_then(|s| match s.slot_ref(profile) {
                Some(ProfileSlot::Open(p)) => Some(p),
                _ => None,
            })
            .and_then(|p| p.controls.keyframe.as_ref());
        match keyframe {
            Some(k) => {
                k.request();
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    "request_keyframe: IDR forced");
            }
            None => {
                // RTSP passthrough (no encoder handle) or no such open tier.
                tracing::debug!(stream = %stream, profile = profile.as_str(),
                    "request_keyframe: no forceable encoder; ignored");
            }
        }
    }

    async fn handle_viewers(&mut self, stream: &str, profile: Profile, matching: bool) {
        let Some(session) = self.sessions.get_mut(stream) else {
            return;
        };
        let Some(ProfileSlot::Open(p)) = session.slot_mut(profile) else {
            return;
        };
        let was = p.viewers;
        p.viewers = matching;
        if matching {
            p.idle_since = None;
            if !was {
                // Rising edge: force a keyframe so the new viewer gets a
                // decodable picture immediately (no-op for JPEG previews).
                if let Some(k) = &p.controls.keyframe {
                    k.request();
                }
                tracing::debug!(stream = %stream, profile = profile.as_str(), "viewer appeared");
            }
        } else if was {
            // Falling edge: start the idle countdown (crash backstop for
            // viewers that die without close_stream).
            p.idle_since = Some(Instant::now());
            tracing::debug!(stream = %stream, profile = profile.as_str(), "last viewer left");
        }

        // Refresh the viewers gauge (profiles with matching subscribers).
        let viewers = u64::from(session.viewers());
        self.stats
            .handle(stream)
            .viewers
            .store(viewers, std::sync::atomic::Ordering::Relaxed);

        // A viewer edge changes a tier's demand signal — republish the
        // per-tier status so the state plane reflects the new viewer counts
        // (the GUI drives its tier picker off this, not just the stats plane).
        if was != matching {
            self.publish_status(stream).await;
        }
    }

    async fn handle_egress_ended(
        &mut self,
        stream: &str,
        profile: Profile,
        epoch: u64,
        end: crate::egress::EgressEnd,
    ) {
        // Stale end report: `open()` already tore this incarnation down (and
        // possibly installed a replacement) — acting on it would kill the
        // replacement, so only the current epoch's report counts.
        let is_current = matches!(
            self.sessions.get(stream).and_then(|s| s.slot_ref(profile)),
            Some(ProfileSlot::Open(p)) if p.epoch == epoch
        );
        if !is_current {
            tracing::debug!(stream = %stream, profile = profile.as_str(), epoch,
                "stale egress-ended for a replaced profile; ignored");
            return;
        }
        // The stall window is the one thing that does not reach the wire, so
        // log it here or it is lost.
        if let crate::egress::EgressEnd::Stalled { window } = &end {
            tracing::warn!(stream = %stream, profile = profile.as_str(),
                window_s = window.as_secs_f64(),
                "no frames within the first-frame window");
        }
        let reason = StreamEndReason::from(end);
        if reason.is_failure() {
            // `Display` is the single source of this prose (#691), so the log
            // line, health's `last_error` and the viewer's tile caption are
            // now literally the same sentence.
            let summary = reason.to_string();
            tracing::warn!(stream = %stream, profile = profile.as_str(), error = %summary,
                "stream profile ended with error");
            if let Some(health) = &self.health {
                health.record_device_failure(stream, &summary);
            }
            // For an RTSP source this *is* the sustained-failure signal
            // (#731): since 0.8 the source retries a dropped stream on its
            // own, so an error reaching here means the whole reconnect
            // ladder (RTSP_MAX_RECONNECTS attempts) ran out. Firing on the
            // first drop would now be noise — a blip that the source healed
            // by itself never gets here at all.
            //
            // The gate is `is_failure()`, not `Failed` alone, and that is what
            // preserves the behaviour: a first-frame stall on an RTSP source
            // already fired this rule when it arrived as an `Err` string.
            if let Some(alerts) = &self.alerts
                && matches!(
                    self.catalog.get(stream).map(|e| &e.kind),
                    Some(SourceKind::Rtsp { .. })
                )
            {
                alerts.rtsp_connect(stream, Some(&summary)).await;
            }
        } else {
            tracing::info!(stream = %stream, profile = profile.as_str(), %reason,
                "stream profile ended");
        }
        self.teardown_profile(stream, profile, reason);
        self.publish_status(stream).await;
    }

    /// Fold every live profile's counters into its stream's [`StreamStats`]:
    /// the sink's shed count and backlog (#692), the encoder's rate-control
    /// drops, and the p95/p99 encode-latency tail (#729).
    ///
    /// Runs on the actor's existing 1 Hz reap tick — finer than the stats
    /// ticker's interval, so the published numbers are at most a second stale.
    /// The actor is the right owner: it already holds `PipelineControls` per
    /// profile and already writes the `viewers` gauge into `StreamStats`.
    fn fold_pipeline_stats(&mut self) {
        // Clone the registry handle so the two field borrows stay disjoint.
        let registry = self.stats.clone();
        for (stream, session) in &mut self.sessions {
            // Iterating `sessions` (not the registry) means `handle`'s
            // create-on-miss can never resurrect a closed stream's entry.
            let stats = registry.handle(stream);
            // Sink and RC drops are summed across profiles (they are counts);
            // the latency tail and the sink backlog are not summable, so the
            // stream reports its **worst live profile** for each. Depth is
            // bounded per sink, so summing three profiles would report a queue
            // that does not exist. Both are recomputed from scratch each tick
            // rather than folded, so a torn-down profile stops being reported.
            let (mut p95_ns, mut p99_ns, mut queue) = (0u64, 0u64, 0u64);
            for slot in session.profiles_mut() {
                if let ProfileSlot::Open(p) = slot {
                    queue = queue.max(fold_profile_counters(&stats, p));
                    if let Some(handle) = &p.controls.encoder_stats {
                        let latency = handle.encode_latency();
                        p95_ns = p95_ns.max(latency.p95_ns);
                        p99_ns = p99_ns.max(latency.p99_ns);
                    }
                }
            }
            stats.set_encode_tail(p95_ns, p99_ns);
            stats.set_sink_queue(queue);
        }
    }

    async fn reap_idle(&mut self) {
        let timeout = Duration::from_secs(self.config.idle_timeout_secs);
        // The refcount travels with the profile: it is the only thing that
        // distinguishes the two ends this one loop produces (#691), and it is
        // gone by the time `teardown_profile` has run.
        let mut reap: Vec<(String, Profile, u32)> = Vec::new();
        for (stream, session) in &self.sessions {
            for (profile, slot) in session.profiles() {
                // Pending slots never idle out here: the bounded RTSP
                // connect always resolves them via `RtspConnected`.
                if let ProfileSlot::Open(p) = slot
                    && !p.viewers
                    && p.idle_since.is_some_and(|t| t.elapsed() >= timeout)
                {
                    reap.push((stream.clone(), profile, p.refcount));
                }
            }
        }
        for (stream, profile, refcount) in reap {
            let reason = idle_reason(refcount);
            tracing::info!(stream = %stream, profile = profile.as_str(), %reason,
                "idle timeout: tearing stream profile down");
            self.teardown_profile(&stream, profile, reason);
            self.publish_status(&stream).await;
        }
    }

    /// Release every OTHER open video tier for `stream` before opening `keep`
    /// (exclusive-source tier switch). A single camera serves one capture at a
    /// time, so the outgoing tier must hand the device over — waiting for its
    /// 30s idle window would leave the new tier stuck on `EBUSY`. Tears down
    /// unconditionally (not just idle/unwatched slots): on an exclusive source
    /// a rival tier can never stream alongside `keep` anyway, and the GUI has
    /// already replaced its tile. The torn-down tier's late `EgressEnded` /
    /// viewer edge are ignored downstream (epoch + slot guards).
    async fn release_conflicting_video_tiers(&mut self, stream: &str, keep: Profile) {
        let siblings: Vec<Profile> = self
            .sessions
            .get(stream)
            .map(|s| {
                s.profiles()
                    .map(|(p, _)| p)
                    .filter(|p| matches!(p, Profile::Video(_)) && *p != keep)
                    .collect()
            })
            .unwrap_or_default();
        if siblings.is_empty() {
            return;
        }
        for profile in siblings {
            tracing::info!(stream = %stream, profile = profile.as_str(),
                "releasing sibling video tier for an exclusive-source tier switch");
            self.teardown_profile(stream, profile, StreamEndReason::Superseded);
        }
        self.publish_status(stream).await;
    }

    /// The configured tier ladder's names, for [`Profile::tier_label`].
    fn tier_names(&self) -> Vec<&str> {
        self.config
            .video
            .tiers
            .iter()
            .map(|t| t.spec.name.as_str())
            .collect()
    }

    /// Record why a tier stopped, so the status published *after* the slot is
    /// gone can still say it.
    ///
    /// Must be called while the profile is still resolvable — the tier name
    /// comes from the profile, and a `Video(idx)` whose ladder entry has been
    /// reconfigured out from under us would otherwise be unnameable.
    fn note_end(&mut self, stream: &str, profile: Profile, reason: StreamEndReason) {
        let tier = profile
            .tier_label(&self.tier_names())
            .unwrap_or_else(|| String::from("unknown"));
        self.last_end
            .insert(stream.to_string(), StreamEnd { tier, reason });
    }

    /// Tear one profile down, recording **why** before the slot that names it
    /// is removed (#691).
    ///
    /// The reason is an argument rather than something inferred here on
    /// purpose: every caller knows what it is doing — reaping, switching
    /// tiers, reacting to a dead pipeline — and only the caller can tell those
    /// apart. Making it an argument is what makes the compiler enumerate the
    /// death paths.
    fn teardown_profile(&mut self, stream: &str, profile: Profile, reason: StreamEndReason) {
        self.note_end(stream, profile, reason);
        let Some(session) = self.sessions.get_mut(stream) else {
            return;
        };
        if let Some(slot) = session.remove_slot(profile) {
            match slot {
                ProfileSlot::Open(mut p) => {
                    // One last fold before the handles die, or a tier switch
                    // silently loses up to a tick's worth of drops.
                    fold_profile_counters(&self.stats.handle(stream), &mut p);
                    p.teardown();
                }
                // A pending reservation has nothing running; dropping it
                // makes the eventual `RtspConnected` a no-op.
                ProfileSlot::Pending(_) => {}
            }
        }
        if session.is_empty() {
            self.sessions.remove(stream);
        }
        self.remove_stats_if_closed(stream);
    }

    /// Drop the stream's stats entry once no profile remains open (the
    /// ticker stops publishing its points).
    fn remove_stats_if_closed(&mut self, stream: &str) {
        if !self.sessions.contains_key(stream) {
            self.stats.remove(stream);
        }
    }

    fn statuses(&self) -> Vec<StreamStatus> {
        self.sessions
            .iter()
            .map(|(stream, session)| self.status_for(stream, session))
            .collect()
    }

    fn status_for(&self, stream: &str, session: &StreamSession) -> StreamStatus {
        use zensight_common::stream::{TierApplied, TierStatus};
        // One `TierStatus` per open video tier — the applied params (what the
        // encoder was actually built with) plus that tier's own viewer count.
        // A pending RTSP connect is not yet a running tier; it counts toward
        // `open` (in progress) but reports no applied params.
        let mut tiers: Vec<TierStatus> = session
            .profiles()
            .filter_map(|(profile, slot)| {
                let idx = profile.tier_index()?;
                let ProfileSlot::Open(p) = slot else {
                    return None;
                };
                Some(TierStatus {
                    tier: self.tier_name(idx).to_string(),
                    applied: TierApplied {
                        width: p.width,
                        height: p.height,
                        fps: self.tier_config(idx).map(|t| t.spec.fps).unwrap_or(0),
                        bitrate_kbps: self
                            .tier_config(idx)
                            .map(|t| t.spec.bitrate_kbps)
                            .unwrap_or(0),
                    },
                    viewers: if p.viewers { 1 } else { 0 },
                })
            })
            .collect();
        tiers.sort_by(|a, b| a.tier.cmp(&b.tier));
        StreamStatus {
            last_end: self.last_end.get(stream).cloned(),
            stream: stream.to_string(),
            open: !session.is_empty(),
            tiers,
        }
    }

    /// Publish the stream's status transition on the declared status
    /// publisher (`@rpc/parallax/streams`) — never a raw `session.put`.
    async fn publish_status(&self, stream: &str) {
        let status = match self.sessions.get(stream) {
            Some(session) => self.status_for(stream, session),
            None => StreamStatus {
                stream: stream.to_string(),
                open: false,
                tiers: Vec::new(),
                // The whole reason `last_end` outlives the session: by the
                // time the last profile is gone there is nothing left to ask.
                last_end: self.last_end.get(stream).cloned(),
            },
        };
        // A stream name is operator-configured, i.e. foreign data, so this
        // is one of the few places zenkey 0.7's reserved-token refusal is
        // genuinely reachable: a stream called `alive` would otherwise mint
        // a key colliding with the liveliness leaf (RFC 03 §3).
        let key = match self.state_ctx.state_key(&["stream", stream]) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(stream, error = %e, "stream name is not a legal state subject");
                return;
            }
        };
        if let Err(e) = self
            .publisher
            .publish_json(&key, &status, QosClass::Command)
            .await
        {
            tracing::warn!(error = %e, "failed to publish stream status");
        }
    }
}

/// How long an RTSP connect may take before the open fails.
const RTSP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times the source retries a dropped RTSP stream before giving up
/// and failing the pipeline.
///
/// **The bound is what makes `rtsp_connect_failed` mean something** (#731).
/// Upstream's default policy is `max_retries: None` — retry forever, which is
/// what a camera wants and what makes a blip invisible. But forever also means
/// a camera that is *gone* never produces an error, so the alert would never
/// fire again after the initial connect and the stream would sit silently
/// "open" with no frames. Bounding the ladder converts sustained failure back
/// into a pipeline error, which arrives as `EgressEnded { error }` and fires
/// the alert through the path that already existed.
///
/// Eight attempts against upstream's 500 ms initial / 30 s ceiling doubling
/// ladder (with full jitter) is roughly a minute and a half of trying before
/// the alert — long enough that a reboot or a switch flap heals silently,
/// short enough that a dead camera is reported while it still matters.
const RTSP_MAX_RECONNECTS: u32 = 8;

/// Connect to an RTSP camera (video track only, bounded).
///
/// Reconnection is **on**: every source in our catalogue is a live camera
/// (`configs/parallax.json5` has no notion of a finite RTSP recording), and
/// upstream retries a clean `Ok(None)` as well as an error under a policy —
/// deliberately, because RTSP has no in-band end-of-stream for a live stream
/// and a server whose process dies looks exactly like one that finished. For a
/// camera that is the right reading. A finite stream — a recording served over
/// RTSP — would want `.without_reconnect()`, which makes an end an end; add
/// that per source kind if such a source is ever configurable.
async fn connect_rtsp(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<RtspSession> {
    let mut src = RtspSrc::new(url)
        .video_only()
        .with_timeout(RTSP_CONNECT_TIMEOUT)
        .with_reconnect(RtspReconnect {
            max_retries: Some(RTSP_MAX_RECONNECTS),
            ..Default::default()
        });
    if let (Some(user), Some(pass)) = (username, password) {
        src = src.with_credentials(user, pass);
    }
    tokio::time::timeout(RTSP_CONNECT_TIMEOUT + Duration::from_secs(1), src.connect())
        .await
        .map_err(|_| anyhow::anyhow!("rtsp connect to {url} timed out"))?
        .map_err(|e| anyhow::anyhow!("rtsp connect to {url} failed: {e}"))
}

/// The video track's SDP dimensions, if advertised.
///
/// Read through an [`RtspStreamInfoHandle`] rather than the session itself:
/// `add_async_source` moves the session into the graph, and this must be
/// callable on either side of that move (#731).
///
/// Still a synchronous, one-shot read of what the SDP carried. Cameras that
/// announce no `a=framesize` and no usable `sprop-parameter-sets` fill their
/// geometry in later, from the first in-band SPS, and the awaitable form for
/// that is `RtspStreamInfoHandle::wait_for_dimensions`. Preview on such a
/// camera still fails to open, exactly as before — making it *wait* means
/// building the preview graph after the pipeline is running, which is a
/// bigger change than this one.
fn rtsp_video_dimensions(info: &RtspStreamInfoHandle) -> Option<(u32, u32)> {
    info.streams()
        .iter()
        .find(|s| s.media_type == RtspMediaType::Video)
        .and_then(|s| s.dimensions)
}

/// Fold one open profile's encoder rate-control drops into its stream's
/// counter. A profile with no encoder in its graph (preview, RTSP passthrough)
/// contributes nothing.
/// Why an idle reap ended a profile (#691).
///
/// A `close_stream` does not stop anything on its own — it releases a
/// refcount, and this countdown does the stopping — so both a clean operator
/// close and the crash backstop arrive through the same reaper. The refcount
/// at reap time is the honest discriminator:
///
/// - **0** — every opener called `close_stream`. The system did what it was
///   told, and the idle window is just how long that takes.
/// - **> 0** — an opener still holds a reference but nothing is watching:
///   nobody ever subscribed, or a viewer died without saying goodbye. The
///   system is cleaning up after something that vanished.
///
/// Those are different events with different follow-ups, and until #691 an
/// operator could see neither.
fn idle_reason(refcount: u32) -> StreamEndReason {
    if refcount == 0 {
        StreamEndReason::Closed
    } else {
        StreamEndReason::Idle
    }
}

/// Fold one profile's live counters into its stream's stats, returning that
/// profile's current sink backlog for the caller's max.
///
/// One `sink.stats()` per profile per second takes the sink's mutex for a few
/// field reads — negligible against a 30 fps push path, and the same lock
/// `pull_buffer_timeout` already takes on every frame.
fn fold_profile_counters(stats: &StreamStats, p: &mut ProfileSession) -> u64 {
    let sink = p.sink.stats();
    stats.fold_sink_drops(&mut p.sink_drops_seen, sink.total_dropped);
    if let Some(handle) = &p.controls.encoder_stats {
        stats.fold_rc_drops(&mut p.rc_drops_seen, handle.frames_dropped_by_rc());
    }
    sink.queued_buffers as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A close and the crash backstop arrive through the same reaper; the
    /// refcount is what tells them apart (#691). Getting this backwards would
    /// report every operator close as an abandoned stream.
    #[test]
    fn the_reaper_tells_a_close_from_an_abandoned_stream() {
        assert_eq!(idle_reason(0), StreamEndReason::Closed);
        assert_eq!(idle_reason(1), StreamEndReason::Idle);
        assert_eq!(idle_reason(7), StreamEndReason::Idle);
        // Neither is a failure: the producer ended these, and device health
        // must not count its own teardowns.
        assert!(!idle_reason(0).is_failure());
        assert!(!idle_reason(1).is_failure());
    }

    /// Every profile must be nameable, or `StreamEnd::tier` falls back to
    /// `unknown` and the per-tier fidelity of the whole field is lost.
    #[test]
    fn every_profile_can_name_its_tier() {
        let ladder = ["low", "medium", "high"];
        assert_eq!(
            Profile::Video(2).tier_label(&ladder).as_deref(),
            Some("high")
        );
        assert_eq!(
            Profile::Preview.tier_label(&ladder).as_deref(),
            Some("preview")
        );
        // The preview needs no ladder at all — it is not a rung.
        assert_eq!(Profile::Preview.tier_label(&[]).as_deref(), Some("preview"));
    }

    #[test]
    fn profile_tier_index() {
        assert_eq!(Profile::Video(0).tier_index(), Some(0));
        assert_eq!(Profile::Video(2).tier_index(), Some(2));
        assert_eq!(Profile::Preview.tier_index(), None);
        // Distinct tiers are distinct map keys (independent encoders).
        assert_ne!(Profile::Video(0), Profile::Video(1));
        assert_eq!(Profile::Video(1), Profile::Video(1));
    }
}
