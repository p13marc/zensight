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

use parallax::elements::{AppSrcHandle, MediaType as RtspMediaType, RtspSession, RtspSrc};
use parallax::pipeline::UnifiedPipelineHandle as PipelineHandle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zenoh::bytes::Encoding;
use zensight_common::QosClass;
use zensight_common::command::status_key;
use zensight_common::stream::{StreamControl, StreamStatus};
use zensight_sensor_core::{Publisher, RawMediaPublisher, SensorHealth};

use crate::alerts::ParallaxAlerts;
use crate::catalog::{Catalog, SourceKind};
use crate::config::ParallaxConfig;
use crate::stats::{StatsRegistry, StreamStats};
use crate::{egress, pipeline};

/// Capacity of the actor's command channel.
const CHANNEL_CAPACITY: usize = 64;

/// One of the two per-stream media profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// `@media/<stream>/video/h264/<profile>` — full-rate H.264.
    Video,
    /// `@media/<stream>/preview/jpeg` — low-fps JPEG previews.
    Preview,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Profile::Video => "video",
            Profile::Preview => "preview",
        }
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
        error: Option<String>,
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
    /// Cooperative source-EOS switch — the only way to end the source's
    /// blocking task (`abort()` alone leaks a live pipeline).
    stop: pipeline::StopHandle,
    keyframe: Option<parallax::elements::codec::KeyframeHandle>,
    egress: JoinHandle<()>,
    matcher: JoinHandle<()>,
    /// RTSP feeder task (pushes camera frames into the pipeline's AppSrc);
    /// `None` for self-driving sources.
    feeder: Option<JoinHandle<()>>,
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
}

impl Drop for ProfileSession {
    fn drop(&mut self) {
        // Belt-and-braces: however this session dies (explicit teardown,
        // actor shutdown, panic unwind, runtime teardown), the source's EOS
        // switch MUST flip — its loop runs on a blocking thread that nothing
        // else can stop, and tokio's shutdown would wait on it forever.
        self.stop.stop();
    }
}

impl ProfileSession {
    fn teardown(mut self) {
        // Order matters: flip the source's EOS switch first (the source loop
        // runs on a blocking thread that abort() cannot cancel), then abort
        // the async plumbing.
        self.stop.stop();
        if let Some(feeder) = self.feeder.take() {
            feeder.abort();
        }
        self.egress.abort();
        self.matcher.abort();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Per-stream pair of optional profile slots.
#[derive(Default)]
struct StreamSession {
    video: Option<ProfileSlot>,
    preview: Option<ProfileSlot>,
}

impl StreamSession {
    fn slot(&mut self, profile: Profile) -> &mut Option<ProfileSlot> {
        match profile {
            Profile::Video => &mut self.video,
            Profile::Preview => &mut self.preview,
        }
    }

    fn slot_ref(&self, profile: Profile) -> Option<&ProfileSlot> {
        match profile {
            Profile::Video => self.video.as_ref(),
            Profile::Preview => self.preview.as_ref(),
        }
    }

    /// The occupied profile slots, in fixed (video, preview) order.
    fn profiles(&self) -> impl Iterator<Item = (Profile, &ProfileSlot)> {
        [
            (Profile::Video, self.video.as_ref()),
            (Profile::Preview, self.preview.as_ref()),
        ]
        .into_iter()
        .filter_map(|(profile, slot)| slot.map(|s| (profile, s)))
    }

    /// Mutable variant of [`Self::profiles`].
    fn profiles_mut(&mut self) -> impl Iterator<Item = (Profile, &mut ProfileSlot)> {
        [
            (Profile::Video, self.video.as_mut()),
            (Profile::Preview, self.preview.as_mut()),
        ]
        .into_iter()
        .filter_map(|(profile, slot)| slot.map(|s| (profile, s)))
    }

    /// Open profiles with matching subscribers (pending slots have none).
    fn viewers(&self) -> u32 {
        self.profiles()
            .filter(|(_, slot)| matches!(slot, ProfileSlot::Open(p) if p.viewers))
            .count() as u32
    }

    fn is_empty(&self) -> bool {
        self.video.is_none() && self.preview.is_none()
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
    status_key: String,
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
        let host_prefix = format!("{}/{}", config.key_prefix, source);
        let manager = SessionManager {
            catalog,
            config,
            source,
            publisher,
            status_key: status_key(&host_prefix, "streams"),
            sessions: HashMap::new(),
            tx: tx.clone(),
            stats,
            health,
            alerts,
            next_epoch: 0,
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
                _ = reap_tick.tick() => self.reap_idle().await,
            }
        }
        // Actor shutdown: tear every remaining profile down (pending
        // reservations have nothing running; dropping them is enough).
        for (_, mut session) in self.sessions.drain() {
            for slot in [session.video.take(), session.preview.take()]
                .into_iter()
                .flatten()
            {
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
            } => self.handle_viewers(&stream, profile, matching),
            SessionMsg::EgressEnded {
                stream,
                profile,
                epoch,
                error,
            } => {
                self.handle_egress_ended(&stream, profile, epoch, error)
                    .await
            }
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

    async fn handle_control(&mut self, control: StreamControl) {
        match control {
            StreamControl::OpenStream {
                stream,
                codec,
                max_height,
            } => {
                let Some(profile) = profile_for_codec(codec.as_deref()) else {
                    tracing::warn!(stream = %stream, codec = ?codec, "open_stream: unsupported codec");
                    return;
                };
                self.open(&stream, profile, max_height).await;
            }
            StreamControl::CloseStream { stream } => self.close(&stream).await,
            StreamControl::RequestKeyframe { stream } => self.request_keyframe(&stream),
        }
    }

    async fn open(&mut self, stream: &str, profile: Profile, max_height: Option<u32>) {
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
            .and_then(|s| s.slot(profile).as_mut())
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
                if let Some(k) = &existing.keyframe {
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
                self.teardown_profile(stream, profile);
            }
            Existing::None => {}
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
                *self
                    .sessions
                    .entry(stream.to_string())
                    .or_default()
                    .slot(profile) = Some(ProfileSlot::Pending(PendingOpen { refcount: 1 }));
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
                    Profile::Video => {
                        pipeline::build_video(&kind, &self.config.video, max_height, &stream_stats)
                    }
                    Profile::Preview => {
                        pipeline::build_preview(&kind, &self.config.preview, &stream_stats)
                    }
                };
                self.finish_open(stream, profile, built, None, stream_stats, 1)
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
        let dims = rtsp_video_dimensions(&rtsp);
        let stream_stats = self.stats.handle(stream);
        let built = match profile {
            Profile::Video => pipeline::build_rtsp_video_passthrough(dims),
            Profile::Preview => match dims {
                Some((w, h)) => {
                    pipeline::build_rtsp_preview(w, h, &self.config.preview, &stream_stats)
                }
                None => Err(anyhow::anyhow!(
                    "rtsp stream advertises no dimensions; preview needs the SDP size"
                )),
            },
        };
        self.finish_open(stream, profile, built, Some(*rtsp), stream_stats, refcount)
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
        rtsp: Option<RtspSession>,
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

        // Declare the media publisher on the profile's concrete key.
        let key = {
            let ctx = self.publisher.v1();
            match profile {
                Profile::Video => ctx.media_video_key(stream, "h264", &self.config.video.profile),
                Profile::Preview => ctx.media_preview_key(stream),
            }
        };
        let media = match self.publisher.raw_media_publisher(key.clone()).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                built.stop.stop();
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
                    built.stop.stop();
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
                built.stop.stop();
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
                Profile::Video => (Encoding::VIDEO_H264, false),
                Profile::Preview => (Encoding::IMAGE_JPEG, true),
            };
            let egress_stats = stream_stats.clone();
            tokio::spawn(async move {
                let result =
                    egress::run(sink, media, encoding, width, height, preview, egress_stats).await;
                let _ = tx
                    .send(SessionMsg::EgressEnded {
                        stream,
                        profile,
                        epoch,
                        error: result.err(),
                    })
                    .await;
            })
        };

        // RTSP: pump the camera session into the pipeline's AppSrc.
        let feeder = rtsp.map(|rtsp_session| {
            let feed = built
                .feed
                .take()
                .expect("rtsp pipelines always carry a feed handle");
            let stream = stream.to_string();
            tokio::spawn(rtsp_feed(rtsp_session, feed, stream))
        });

        // First IDR right away so an already-waiting viewer decodes at once.
        if let Some(k) = &built.keyframe {
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
        *self
            .sessions
            .entry(stream.to_string())
            .or_default()
            .slot(profile) = Some(ProfileSlot::Open(Box::new(ProfileSession {
            handle: Some(handle),
            stop: built.stop,
            keyframe: built.keyframe,
            egress,
            matcher,
            feeder,
            publisher: media,
            refcount,
            epoch,
            viewers,
            // Unwatched until a viewer actually subscribes: give the opener
            // one idle window to show up, then reap (zombie-open backstop).
            idle_since: if viewers { None } else { Some(Instant::now()) },
        })));
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
        self.clear_pending_slot(stream, profile);
        self.remove_stats_if_closed(stream);
        self.publish_status(stream).await;
    }

    /// Remove a pending reservation (never an `Open` slot — those must go
    /// through [`Self::teardown_profile`] so the pipeline's stop switch
    /// flips), dropping the stream entry when both slots are empty.
    fn clear_pending_slot(&mut self, stream: &str, profile: Profile) {
        if let Some(session) = self.sessions.get_mut(stream) {
            if matches!(session.slot(profile), Some(ProfileSlot::Pending(_))) {
                *session.slot(profile) = None;
            }
            if session.is_empty() {
                self.sessions.remove(stream);
            }
        }
    }

    /// `close_stream` carries no codec: decrement every open profile of the
    /// stream (an open/close pair from one requester is symmetric per key).
    async fn close(&mut self, stream: &str) {
        let Some(session) = self.sessions.get_mut(stream) else {
            tracing::debug!(stream = %stream, "close_stream: not open");
            return;
        };
        for (profile, slot) in session.profiles_mut() {
            match slot {
                ProfileSlot::Pending(p) => {
                    // Still connecting: `RtspConnected` discards the camera
                    // session if the refcount is 0 by the time it resolves.
                    p.refcount = p.refcount.saturating_sub(1);
                    tracing::debug!(stream = %stream, profile = profile.as_str(),
                        refcount = p.refcount, "close_stream: pending refcount decremented");
                }
                ProfileSlot::Open(p) => {
                    p.refcount = p.refcount.saturating_sub(1);
                    if p.refcount == 0 && !p.viewers && p.idle_since.is_none() {
                        p.idle_since = Some(Instant::now());
                    }
                    tracing::debug!(stream = %stream, profile = profile.as_str(),
                        refcount = p.refcount, "close_stream: refcount decremented");
                }
            }
        }
        self.publish_status(stream).await;
    }

    fn request_keyframe(&mut self, stream: &str) {
        let keyframe = self
            .sessions
            .get(stream)
            .and_then(|s| match &s.video {
                Some(ProfileSlot::Open(p)) => Some(p),
                _ => None,
            })
            .and_then(|p| p.keyframe.as_ref());
        match keyframe {
            Some(k) => {
                k.request();
                tracing::debug!(stream = %stream, "request_keyframe: IDR forced");
            }
            None => {
                // RTSP passthrough (no encoder handle) or no open video profile.
                tracing::debug!(stream = %stream, "request_keyframe: no forceable encoder; ignored");
            }
        }
    }

    fn handle_viewers(&mut self, stream: &str, profile: Profile, matching: bool) {
        let Some(session) = self.sessions.get_mut(stream) else {
            return;
        };
        let Some(ProfileSlot::Open(p)) = session.slot(profile).as_mut() else {
            return;
        };
        let was = p.viewers;
        p.viewers = matching;
        if matching {
            p.idle_since = None;
            if !was {
                // Rising edge: force a keyframe so the new viewer gets a
                // decodable picture immediately (no-op for JPEG previews).
                if let Some(k) = &p.keyframe {
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
    }

    async fn handle_egress_ended(
        &mut self,
        stream: &str,
        profile: Profile,
        epoch: u64,
        error: Option<String>,
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
        match &error {
            Some(e) => {
                tracing::warn!(stream = %stream, profile = profile.as_str(), error = %e,
                    "stream profile ended with error");
                if let Some(health) = &self.health {
                    health.record_device_failure(stream, e);
                }
            }
            None => tracing::info!(stream = %stream, profile = profile.as_str(),
                "stream profile reached end of stream"),
        }
        self.teardown_profile(stream, profile);
        self.publish_status(stream).await;
    }

    async fn reap_idle(&mut self) {
        let timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let mut reap: Vec<(String, Profile)> = Vec::new();
        for (stream, session) in &self.sessions {
            for (profile, slot) in session.profiles() {
                // Pending slots never idle out here: the bounded RTSP
                // connect always resolves them via `RtspConnected`.
                if let ProfileSlot::Open(p) = slot
                    && !p.viewers
                    && p.idle_since.is_some_and(|t| t.elapsed() >= timeout)
                {
                    reap.push((stream.clone(), profile));
                }
            }
        }
        for (stream, profile) in reap {
            tracing::info!(stream = %stream, profile = profile.as_str(),
                "idle timeout: tearing stream profile down");
            self.teardown_profile(&stream, profile);
            self.publish_status(&stream).await;
        }
    }

    fn teardown_profile(&mut self, stream: &str, profile: Profile) {
        let Some(session) = self.sessions.get_mut(stream) else {
            return;
        };
        if let Some(slot) = session.slot(profile).take() {
            match slot {
                ProfileSlot::Open(p) => p.teardown(),
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
        let viewers = session.viewers();
        // Active profile string: the video profile wins if open. A pending
        // RTSP connect counts as open (in progress) — only a resolved
        // failure publishes the definitive `open: false` transition.
        let profile = if session.video.is_some() {
            Some(format!("h264/{}", self.config.video.profile))
        } else if session.preview.is_some() {
            Some("mjpeg".to_string())
        } else {
            None
        };
        StreamStatus {
            stream: stream.to_string(),
            open: !session.is_empty(),
            viewers,
            profile,
        }
    }

    /// Publish the stream's status transition on the declared status
    /// publisher (`@/status/streams`) — never a raw `session.put`.
    async fn publish_status(&self, stream: &str) {
        let status = match self.sessions.get(stream) {
            Some(session) => self.status_for(stream, session),
            None => StreamStatus {
                stream: stream.to_string(),
                open: false,
                viewers: 0,
                profile: None,
            },
        };
        if let Err(e) = self
            .publisher
            .publish_json(&self.status_key, &status, QosClass::Command)
            .await
        {
            tracing::warn!(error = %e, "failed to publish stream status");
        }
    }
}

/// How long an RTSP connect may take before the open fails.
const RTSP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to an RTSP camera (video track only, bounded).
async fn connect_rtsp(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<RtspSession> {
    let mut src = RtspSrc::new(url)
        .video_only()
        .with_timeout(RTSP_CONNECT_TIMEOUT);
    if let (Some(user), Some(pass)) = (username, password) {
        src = src.with_credentials(user, pass);
    }
    tokio::time::timeout(RTSP_CONNECT_TIMEOUT + Duration::from_secs(1), src.connect())
        .await
        .map_err(|_| anyhow::anyhow!("rtsp connect to {url} timed out"))?
        .map_err(|e| anyhow::anyhow!("rtsp connect to {url} failed: {e}"))
}

/// The video track's SDP dimensions, if advertised.
fn rtsp_video_dimensions(session: &RtspSession) -> Option<(u32, u32)> {
    session
        .streams()
        .iter()
        .find(|s| s.media_type == RtspMediaType::Video)
        .and_then(|s| s.dimensions)
}

/// Pump RTSP frames into the pipeline's `AppSrc` until the camera ends or
/// errors, shedding frames when the pipeline is busy (live video must never
/// back the network reader up). Ends the app stream on exit so the pipeline
/// unwinds and the egress task reports `EgressEnded`.
async fn rtsp_feed(mut rtsp: RtspSession, feed: AppSrcHandle, stream: String) {
    loop {
        match rtsp.produce().await {
            Ok(Some(buffer)) => {
                if feed.is_full() {
                    continue; // shed the frame
                }
                if feed
                    .push_buffer_timeout(buffer, Some(Duration::from_millis(50)))
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => {
                tracing::info!(stream = %stream, "rtsp source reached end of stream");
                break;
            }
            Err(e) => {
                tracing::warn!(stream = %stream, error = %e, "rtsp source failed");
                break;
            }
        }
    }
    feed.end_stream();
}

/// Map a requested codec onto a profile (`None` = sensor default = video).
fn profile_for_codec(codec: Option<&str>) -> Option<Profile> {
    match codec {
        None | Some("h264") => Some(Profile::Video),
        Some("mjpeg") | Some("jpeg") => Some(Profile::Preview),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_maps_to_profile() {
        assert_eq!(profile_for_codec(None), Some(Profile::Video));
        assert_eq!(profile_for_codec(Some("h264")), Some(Profile::Video));
        assert_eq!(profile_for_codec(Some("mjpeg")), Some(Profile::Preview));
        assert_eq!(profile_for_codec(Some("jpeg")), Some(Profile::Preview));
        assert_eq!(profile_for_codec(Some("av1")), None);
    }
}
