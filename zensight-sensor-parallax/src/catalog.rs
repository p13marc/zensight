//! The stream catalogue: which video sources this host advertises.
//!
//! Seeded at startup by merging three source families, and **live thereafter**
//! (#410): the hotplug watcher adds a `SourceKind::V4l2` entry when a capture
//! device appears and removes it when the device is pulled, so a USB camera
//! plugged into a running sensor is advertised without a restart.
//!
//! That is why the entries sit behind a lock rather than being handed out as a
//! `&[CatalogEntry]`: the catalogue is shared by four readers (the `streams`
//! queryable, the session actor, the health device count and the alerts) and
//! one writer. Every accessor clones what it returns and drops the guard
//! immediately — nothing holds it across an await.
//!
//! Seeded from:
//! - enumerated local V4L2 cameras (`enumerate_v4l2: true`),
//! - configured remote RTSP cameras,
//! - configured synthetic test-pattern sources (demo mode / CI).
//!
//! Every entry becomes one `StreamDescriptor` on the `@rpc/parallax/streams`
//! catalogue and one `state/parallax/device/<stream>/alive` liveliness token; the
//! `<stream>` name is the key chunk under `@media/parallax/`.

use std::collections::HashSet;
use std::sync::RwLock;

use zensight_common::stream::{StreamDescriptor, TierSpec};

use crate::config::{ParallaxConfig, RtspSourceConfig, TestSourceConfig};

/// How to reach one video source (drives pipeline construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Synthetic `VideoTestSrc` pattern (identical downstream path to a real
    /// camera — demo mirrors the contract).
    Test {
        pattern: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Local V4L2 camera.
    V4l2 {
        /// Device path (e.g. `/dev/video0`).
        device: String,
    },
    /// Remote RTSP camera.
    Rtsp {
        url: String,
        username: Option<String>,
        password: Option<String>,
    },
}

impl SourceKind {
    /// Whether this source can be captured by only ONE pipeline at a time.
    ///
    /// A single V4L2 device (`/dev/videoX`) can't be streamed by two pipelines
    /// at once — the second `REQBUFS`/`S_FMT` fails `EBUSY` — and most RTSP
    /// cameras cap concurrent sessions. The synthetic test source has no such
    /// limit (each pipeline generates independently). This gates whether
    /// opening a new video tier must first release a sibling tier's capture
    /// (see `SessionManager::open`): exclusive sources serve one video tier per
    /// stream, shareable sources allow concurrent tiers (true simulcast).
    pub fn is_exclusive(&self) -> bool {
        matches!(self, SourceKind::V4l2 { .. } | SourceKind::Rtsp { .. })
    }
}

/// One advertised stream: name + how to open it + its native capabilities.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Stream identifier (single key chunk, unique within the catalogue).
    pub name: String,
    pub kind: SourceKind,
    /// Native capture width/height/framerate, when known (`None` for an RTSP
    /// source whose SDP we have not read, or a V4L2 device we could not probe).
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    /// Codecs this source can be opened with — probed per source, not a
    /// hardcoded pair (#507).
    pub codecs: Vec<String>,
    /// Human-readable description (camera model / pattern).
    pub description: Option<String>,
}

/// The full, ordered stream catalogue for this host.
///
/// Cheap to read and rarely written: a hotplug event is a human plugging in a
/// camera, while reads happen per `streams` query and per stream open.
#[derive(Debug, Default)]
pub struct Catalog {
    entries: RwLock<Vec<CatalogEntry>>,
}

impl Catalog {
    /// Build the catalogue from config: enumerated V4L2 devices, then RTSP,
    /// then test sources. Later families skip names already taken (config
    /// validation already guarantees rtsp/test uniqueness among themselves).
    pub fn build(config: &ParallaxConfig) -> Self {
        let mut entries: Vec<CatalogEntry> = Vec::new();
        let mut names: HashSet<String> = HashSet::new();

        if config.enumerate_v4l2 {
            match parallax::elements::device::enumerate_video_devices() {
                Ok(devices) => {
                    for dev in devices {
                        let name = v4l2_stream_name(&dev.id);
                        if !names.insert(name.clone()) {
                            tracing::warn!(device = %dev.id, stream = %name,
                                "v4l2 device name collides with an existing stream; skipped");
                            continue;
                        }
                        let description = device_description(&dev.name, &dev.model);
                        // Best-effort native-capability probe: open the device,
                        // read its negotiated geometry, drop it (releasing the
                        // camera). A busy/failing device just advertises no
                        // native size — honest "unknown until opened".
                        let (width, height, fps) = probe_v4l2(&dev.id);
                        entries.push(CatalogEntry {
                            name,
                            kind: SourceKind::V4l2 {
                                device: dev.id.clone(),
                            },
                            width,
                            height,
                            fps,
                            codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                            description: Some(description),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "v4l2 enumeration failed; no local cameras advertised");
                }
            }
        }

        for rtsp in &config.rtsp {
            if !names.insert(rtsp.name.clone()) {
                tracing::warn!(stream = %rtsp.name, "rtsp stream name collides; skipped");
                continue;
            }
            entries.push(rtsp_entry(rtsp));
        }

        for test in &config.test_sources {
            if !names.insert(test.name.clone()) {
                tracing::warn!(stream = %test.name, "test stream name collides; skipped");
                continue;
            }
            entries.push(test_entry(test));
        }

        Self {
            entries: RwLock::new(entries),
        }
    }

    /// Look one stream up by name.
    ///
    /// Returns a clone: the entry may be removed by a hotplug event between
    /// this call and its use, and a caller holding a reference into the
    /// catalogue would keep the lock (or, worse, want to) across an await.
    pub fn get(&self, name: &str) -> Option<CatalogEntry> {
        self.read().iter().find(|e| e.name == name).cloned()
    }

    /// All entries, in catalogue order.
    pub fn entries(&self) -> Vec<CatalogEntry> {
        self.read().clone()
    }

    /// The advertised stream names, in catalogue order.
    pub fn stream_names(&self) -> Vec<String> {
        self.read().iter().map(|e| e.name.clone()).collect()
    }

    /// How many streams are advertised.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether the catalogue advertises nothing at all.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The V4L2 device paths currently advertised, with their stream names.
    pub fn v4l2_devices(&self) -> Vec<(String, String)> {
        self.read()
            .iter()
            .filter_map(|e| match &e.kind {
                SourceKind::V4l2 { device } => Some((e.name.clone(), device.clone())),
                _ => None,
            })
            .collect()
    }

    /// Add a hotplugged V4L2 camera (#410). Returns the stream name when it was
    /// added, `None` when the catalogue already advertises that device or the
    /// name it would take.
    ///
    /// Both refusals are ordinary, not errors. udev re-announces devices that
    /// were already present, and a `/dev/video*` path freed by an unplug can be
    /// handed to a different camera by the kernel — so "already there" and
    /// "name taken by something else" are both states a running fleet reaches.
    pub fn add_v4l2(&self, device: &str, description: Option<String>) -> Option<String> {
        let name = v4l2_stream_name(device);
        let mut entries = self.write();
        if entries.iter().any(|e| e.name == name) {
            return None;
        }
        let (width, height, fps) = probe_v4l2(device);
        entries.push(CatalogEntry {
            name: name.clone(),
            kind: SourceKind::V4l2 {
                device: device.to_string(),
            },
            width,
            height,
            fps,
            codecs: vec!["h264".to_string(), "mjpeg".to_string()],
            description,
        });
        Some(name)
    }

    /// Drop the entry for an unplugged V4L2 device (#410), returning its stream
    /// name if it was advertised.
    ///
    /// `None` is expected and must stay quiet: upstream documents that a
    /// removal event cannot be capability-checked — the device is already gone
    /// — so a `Removed` arrives for ids that never produced an `Added`,
    /// including every metadata-only node a UVC camera exposes beside its
    /// capture node.
    pub fn remove_v4l2(&self, device: &str) -> Option<String> {
        let mut entries = self.write();
        let idx = entries.iter().position(|e| match &e.kind {
            SourceKind::V4l2 { device: d } => d == device,
            _ => false,
        })?;
        Some(entries.remove(idx).name)
    }

    // A poisoned lock means a reader panicked while holding it. The catalogue
    // is a plain Vec of owned data with no invariant a panic could have half-
    // applied, so recovering the guard is strictly better than propagating a
    // panic into every stream open for the life of the process.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<CatalogEntry>> {
        self.entries.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<CatalogEntry>> {
        self.entries.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Serve-ready descriptors, stamping `active` from the currently open set
    /// and the tiers each stream offers from the sensor's `ladder` (filtered to
    /// what the camera can actually feed — no 720p tier for a 480p camera).
    pub fn descriptors(
        &self,
        open: &HashSet<String>,
        ladder: &[TierSpec],
    ) -> Vec<StreamDescriptor> {
        self.read()
            .iter()
            .map(|e| StreamDescriptor {
                stream: e.name.clone(),
                codecs: e.codecs.clone(),
                active: open.contains(&e.name),
                width: e.width,
                height: e.height,
                fps: e.fps,
                tiers: offered_tiers(e.height, ladder),
                description: e.description.clone(),
            })
            .collect()
    }
}

/// The tiers a source with native height `native_h` can honestly offer: a tier
/// with a fixed `max_height` above the camera's native height would only be
/// upscaled (the scaler never upscales), so drop it. A `None`-capped tier (use
/// native) and every tier when the native size is unknown are always offered.
fn offered_tiers(native_h: Option<u32>, ladder: &[TierSpec]) -> Vec<TierSpec> {
    ladder
        .iter()
        .filter(|t| match (native_h, t.max_height) {
            (Some(nh), Some(mh)) => mh <= nh,
            _ => true,
        })
        .cloned()
        .collect()
}

/// Best-effort probe of a V4L2 device's native geometry (open → read → drop).
fn probe_v4l2(device: &str) -> (Option<u32>, Option<u32>, Option<f32>) {
    match parallax::elements::V4l2Src::new(device) {
        Ok(src) => {
            let fps = src
                .framerate()
                .map(|(num, den)| num as f32 / den.max(1) as f32);
            (Some(src.width()), Some(src.height()), fps)
        }
        Err(e) => {
            tracing::debug!(device = %device, error = %e,
                "v4l2 capability probe failed; advertising no native size");
            (None, None, None)
        }
    }
}

fn rtsp_entry(rtsp: &RtspSourceConfig) -> CatalogEntry {
    CatalogEntry {
        name: rtsp.name.clone(),
        kind: SourceKind::Rtsp {
            url: rtsp.url.clone(),
            username: rtsp.username.clone(),
            password: rtsp.password.clone(),
        },
        // Native size is unknown until the SDP is read at connect; the preview
        // (JPEG) needs those dims, so RTSP advertises only h264 passthrough.
        width: None,
        height: None,
        fps: None,
        codecs: vec!["h264".to_string()],
        // Never leak credentials: description is either the configured text
        // or the bare URL (which the operator wrote without inline creds).
        description: rtsp.description.clone().or_else(|| Some(rtsp.url.clone())),
    }
}

fn test_entry(test: &TestSourceConfig) -> CatalogEntry {
    CatalogEntry {
        name: test.name.clone(),
        kind: SourceKind::Test {
            pattern: test.pattern.clone(),
            width: test.width,
            height: test.height,
            fps: test.fps,
        },
        // A synthetic source's geometry is exactly its config — no probe needed.
        width: Some(test.width),
        height: Some(test.height),
        fps: Some(test.fps as f32),
        codecs: vec!["h264".to_string(), "mjpeg".to_string()],
        description: Some(format!("test pattern {}", test.pattern)),
    }
}

/// One human label for a capture device: its name, plus the model when the
/// model says something the name does not. Shared by startup enumeration and
/// the hotplug watcher so a camera reads identically whichever found it.
pub(crate) fn device_description(name: &str, model: &Option<String>) -> String {
    match model {
        Some(model) if model != name => format!("{name} ({model})"),
        _ => name.to_string(),
    }
}

/// Derive a stream name from a V4L2 device path: `/dev/video0` → `video0`.
fn v4l2_stream_name(device_id: &str) -> String {
    device_id
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(device_id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ParallaxConfig {
        let json = r#"{
            enumerate_v4l2: false,
            rtsp: [
                { name: "door", url: "rtsp://cam.local/1", description: "front door" },
                { name: "yard", url: "rtsp://cam.local/2" },
            ],
            test_sources: [
                { name: "test0", pattern: "smpte", width: 640, height: 360, fps: 15 },
            ],
        }"#;
        json5::from_str(json).unwrap()
    }

    #[test]
    fn build_merges_rtsp_and_test_sources() {
        let catalog = Catalog::build(&test_config());
        let names = catalog.stream_names();
        assert_eq!(names, vec!["door", "yard", "test0"]);

        assert!(matches!(
            &catalog.get("door").unwrap().kind,
            SourceKind::Rtsp { url, .. } if url == "rtsp://cam.local/1"
        ));
        // Undescribed RTSP entries fall back to the URL.
        assert_eq!(
            catalog.get("yard").unwrap().description.as_deref(),
            Some("rtsp://cam.local/2")
        );
        assert!(matches!(
            &catalog.get("test0").unwrap().kind,
            SourceKind::Test {
                width: 640,
                height: 360,
                fps: 15,
                ..
            }
        ));
        assert!(catalog.get("nope").is_none());
    }

    #[test]
    fn descriptors_stamp_active_from_open_set() {
        let catalog = Catalog::build(&test_config());
        let open: HashSet<String> = ["test0".to_string()].into();
        let ladder = vec![
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
        ];
        let descs = catalog.descriptors(&open, &ladder);
        assert_eq!(descs.len(), 3);
        for d in &descs {
            assert_eq!(d.active, d.stream == "test0");
        }
        // RTSP advertises h264-only (passthrough); the test source advertises
        // both codecs and its native geometry (no more "resolution rides the
        // description").
        let door = descs.iter().find(|d| d.stream == "door").unwrap();
        assert_eq!(door.codecs, vec!["h264"]);
        assert_eq!(door.width, None);
        let test0 = descs.iter().find(|d| d.stream == "test0").unwrap();
        assert_eq!(test0.codecs, vec!["h264", "mjpeg"]);
        assert_eq!((test0.width, test0.height), (Some(640), Some(360)));
        // A 360-high source is offered both tiers (240 fits, high is uncapped).
        let tier_names: Vec<&str> = test0.tiers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tier_names, vec!["low", "high"]);
    }

    // ── hotplug (#410): the catalogue is live ────────────────────────────

    #[test]
    fn hotplug_adds_and_removes_a_camera() {
        let catalog = Catalog::build(&test_config());
        assert_eq!(catalog.stream_names(), vec!["door", "yard", "test0"]);

        let added = catalog.add_v4l2("/dev/video9", Some("USB cam".into()));
        assert_eq!(added.as_deref(), Some("video9"));
        assert_eq!(catalog.len(), 4);
        assert!(matches!(
            catalog.get("video9").unwrap().kind,
            SourceKind::V4l2 { ref device } if device == "/dev/video9"
        ));
        // The entry a viewer would be offered, not just one the map knows.
        let descs = catalog.descriptors(&HashSet::new(), &[]);
        assert!(descs.iter().any(|d| d.stream == "video9"));

        assert_eq!(
            catalog.remove_v4l2("/dev/video9").as_deref(),
            Some("video9")
        );
        assert_eq!(catalog.stream_names(), vec!["door", "yard", "test0"]);
        assert!(catalog.get("video9").is_none());
    }

    #[test]
    fn a_device_is_only_added_once() {
        // udev re-announces devices that are already present. The second
        // announcement must not produce a duplicate stream — which would
        // publish two liveliness tokens for one camera and double the health
        // device count.
        let catalog = Catalog::build(&test_config());
        assert!(catalog.add_v4l2("/dev/video0", None).is_some());
        assert!(catalog.add_v4l2("/dev/video0", None).is_none());
        assert_eq!(catalog.len(), 4);
    }

    #[test]
    fn a_hotplugged_name_never_displaces_a_configured_stream() {
        // /dev/video* paths are recycled by the kernel, and a configured RTSP
        // or test stream could be called `video0`. Silently replacing it would
        // point an operator's stream at a camera they did not configure.
        let catalog = Catalog::build(&test_config());
        assert!(catalog.add_v4l2("/dev/door", None).is_none());
        assert!(matches!(
            catalog.get("door").unwrap().kind,
            SourceKind::Rtsp { .. }
        ));
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn removing_an_unknown_device_is_a_no_op() {
        // Upstream: a removal cannot be capability-checked, so udev reports
        // every vanished /dev/video* node — including the metadata-only second
        // node a UVC camera exposes, which was never advertised. This is the
        // ordinary case, not an error, and it must not disturb the catalogue.
        let catalog = Catalog::build(&test_config());
        assert!(catalog.remove_v4l2("/dev/video42").is_none());
        assert_eq!(catalog.stream_names(), vec!["door", "yard", "test0"]);
    }

    #[test]
    fn removal_matches_the_device_path_not_the_stream_name() {
        // The two are related by `v4l2_stream_name` and are NOT the same
        // string; a lookup by name would silently fail to remove anything, and
        // an unplugged camera would stay advertised forever.
        let catalog = Catalog::build(&test_config());
        catalog.add_v4l2("/dev/video3", None);
        assert!(catalog.remove_v4l2("video3").is_none());
        assert_eq!(
            catalog.remove_v4l2("/dev/video3").as_deref(),
            Some("video3")
        );
    }

    #[test]
    fn only_v4l2_entries_are_removable_by_device() {
        // An RTSP entry whose URL happened to equal a device path must not be
        // removable by a hotplug event: the kinds are what distinguish them.
        let catalog = Catalog::build(&test_config());
        assert!(catalog.remove_v4l2("rtsp://cam.local/1").is_none());
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn device_description_folds_in_the_model_only_when_it_adds_something() {
        assert_eq!(device_description("HD Webcam", &None), "HD Webcam");
        assert_eq!(
            device_description("HD Webcam", &Some("HD Webcam".into())),
            "HD Webcam"
        );
        assert_eq!(
            device_description("HD Webcam", &Some("Acme C1".into())),
            "HD Webcam (Acme C1)"
        );
    }

    #[test]
    fn v4l2_names_derive_from_device_path() {
        assert_eq!(v4l2_stream_name("/dev/video0"), "video0");
        assert_eq!(v4l2_stream_name("video7"), "video7");
    }

    #[test]
    fn exclusive_sources_are_single_capture() {
        // A single camera can't feed two captures at once → one video tier at a
        // time (drives the tier-switch hand-over). The test source is shareable.
        assert!(
            SourceKind::V4l2 {
                device: "/dev/video0".into()
            }
            .is_exclusive()
        );
        assert!(
            SourceKind::Rtsp {
                url: "rtsp://cam/1".into(),
                username: None,
                password: None,
            }
            .is_exclusive()
        );
        assert!(
            !SourceKind::Test {
                pattern: "smpte".into(),
                width: 320,
                height: 240,
                fps: 30,
            }
            .is_exclusive()
        );
    }

    #[test]
    fn enumeration_is_safe_headless() {
        // On a camera-less host enumerate_v4l2 must contribute nothing and
        // not fail the build.
        let mut config = test_config();
        config.enumerate_v4l2 = true;
        let catalog = Catalog::build(&config);
        // The configured streams are always present; any real cameras on the
        // host would only add to them.
        assert!(catalog.get("door").is_some());
        assert!(catalog.get("test0").is_some());
    }
}
