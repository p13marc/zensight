//! Container fleet overview — patch drift, OOM kills, restarts and pressure.
//!
//! #1128: `image_behind_upstream`, `oom_kills_total`, `restart_count` and the
//! three `*_pressure_avg10` gauges have been on the bus since #819 and reached
//! a generic key/value row on a device card. The fleet question — *these seven
//! containers run an image behind its upstream digest* — is one table, and had
//! no home.
//!
//! **Not checked is not up to date.** `image_behind_upstream` is published only
//! when the collector actually resolved the tag's current upstream digest
//! (`zensight-sensor-container/src/poller.rs:322` — `if
//! c.image.upstream_digest.is_some()`), which needs the explicitly-egressing
//! collector switched on. Its absence therefore means *nobody looked*, and a
//! patch-drift list that quietly counted unchecked containers as current would
//! be worse than no list: it would answer the question wrongly, with
//! confidence. The unchecked ones get their own count, beside the drifted, and
//! neither is folded into the other.
//!
//! `healthy` has the same shape one subject up — absent when no healthcheck is
//! configured or none has ever run — and is treated the same way.

use std::collections::{BTreeMap, HashMap};

use iced::widget::{Column, column, row, text};
use iced::{Alignment, Element, Theme};

use zensight_common::TelemetryValue;
use zensight_common::registry::container::Subject;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// What the sensor was able to say about one container's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageStanding {
    /// Running digest differs from the tag's current upstream digest.
    Behind,
    /// Checked, and current.
    Current,
    /// **Not checked** — the egressing collector is off, or the digest could
    /// not be resolved. Never rendered as "current".
    Unchecked,
}

/// One container's fleet row.
#[derive(Debug, Clone)]
pub struct ContainerRow {
    /// `host/name` — a container name is unique per host, not per fleet.
    pub label: String,
    pub image: ImageStanding,
    pub oom_kills: u64,
    pub restarts: u64,
    /// The worst of the three PSI `avg10` gauges, and which one it was.
    pub worst_pressure: Option<(&'static str, f64)>,
    pub running: bool,
}

/// Fleet counts. `unchecked` is deliberately not folded into either of the
/// other two — see the module note.
#[derive(Debug, Default, Clone, Copy)]
pub struct ContainerAgg {
    pub total: usize,
    pub running: usize,
    pub behind: usize,
    pub unchecked: usize,
    pub with_oom_kills: usize,
}

fn num(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(c) => Some(*c as f64),
        TelemetryValue::Gauge(g) => Some(*g),
        TelemetryValue::Boolean(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

#[derive(Default)]
struct Raw {
    behind: Option<bool>,
    oom_kills: u64,
    restarts: u64,
    cpu_psi: Option<f64>,
    mem_psi: Option<f64>,
    io_psi: Option<f64>,
    running: bool,
}

/// Build one row per container across every host.
#[must_use]
pub fn container_rows(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<ContainerRow> {
    let mut raw: BTreeMap<String, Raw> = BTreeMap::new();

    for (id, state) in devices {
        // A container name is unique on its host and nowhere else: two hosts
        // each running `redis` are two containers, and merging them on the
        // bare name would report one host's OOM kills against the other's.
        let host = id.source.clone();
        for (key, point) in &state.metrics {
            let Some(subject) = Subject::parse_metric(key) else {
                continue;
            };
            let v = num(&point.value);
            // A macro, not a closure: a closure returning `&mut` into `raw`
            // cannot be `FnMut` and be called from several match arms.
            macro_rules! at {
                ($name:expr) => {
                    raw.entry(format!("{host}/{}", $name.as_str())).or_default()
                };
            }
            match subject {
                Subject::ImageBehindUpstream { name } => {
                    at!(name).behind = v.map(|n| n != 0.0);
                }
                Subject::OomKillsTotal { name } => {
                    at!(name).oom_kills = v.unwrap_or(0.0) as u64;
                }
                Subject::RestartCount { name } => {
                    at!(name).restarts = v.unwrap_or(0.0) as u64;
                }
                Subject::CpuPressureAvg10 { name } => at!(name).cpu_psi = v,
                Subject::MemoryPressureAvg10 { name } => at!(name).mem_psi = v,
                Subject::IoPressureAvg10 { name } => at!(name).io_psi = v,
                Subject::Running { name } => {
                    at!(name).running = v.is_some_and(|n| n != 0.0);
                }
                // Any per-container subject earns a row: a container the
                // sensor sees but cannot check the image of still belongs in
                // the count of what was not checked.
                Subject::MemoryBytes { name } | Subject::Pids { name } => {
                    let _ = at!(name);
                }
                _ => {}
            }
        }
    }

    let mut rows: Vec<ContainerRow> = raw
        .into_iter()
        .map(|(label, r)| {
            let worst = [("cpu", r.cpu_psi), ("memory", r.mem_psi), ("io", r.io_psi)]
                .into_iter()
                .filter_map(|(n, v)| v.map(|v| (n, v)))
                .max_by(|a, b| a.1.total_cmp(&b.1));
            ContainerRow {
                label,
                image: match r.behind {
                    Some(true) => ImageStanding::Behind,
                    Some(false) => ImageStanding::Current,
                    None => ImageStanding::Unchecked,
                },
                oom_kills: r.oom_kills,
                restarts: r.restarts,
                worst_pressure: worst,
                running: r.running,
            }
        })
        .collect();

    // Drifted first, then OOM kills, then restarts — the order an operator
    // would triage in. `Unchecked` sorts with `Current`, because an unchecked
    // container is not a finding; the *count* of them is.
    rows.sort_by(|a, b| {
        let k = |r: &ContainerRow| {
            (
                u8::from(r.image != ImageStanding::Behind),
                std::cmp::Reverse(r.oom_kills),
                std::cmp::Reverse(r.restarts),
            )
        };
        k(a).cmp(&k(b)).then_with(|| a.label.cmp(&b.label))
    });
    rows
}

/// Fold the rows into the fleet counts.
#[must_use]
pub fn aggregate(rows: &[ContainerRow]) -> ContainerAgg {
    ContainerAgg {
        total: rows.len(),
        running: rows.iter().filter(|r| r.running).count(),
        behind: rows
            .iter()
            .filter(|r| r.image == ImageStanding::Behind)
            .count(),
        unchecked: rows
            .iter()
            .filter(|r| r.image == ImageStanding::Unchecked)
            .count(),
        with_oom_kills: rows.iter().filter(|r| r.oom_kills > 0).count(),
    }
}

/// Render the container overview.
pub fn container_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return muted("No container hosts available");
    }

    let rows = container_rows(devices);
    let agg = aggregate(&rows);

    let mut col = Column::new().spacing(space::SM);

    col = col.push(
        row![
            stat("Containers", format!("{}/{}", agg.running, agg.total)),
            stat("Image behind upstream", agg.behind.to_string()),
            stat("OOM-killed", agg.with_oom_kills.to_string()),
        ]
        .spacing(space::LG)
        .align_y(Alignment::Center),
    );

    // Stated, never folded. "0 behind" over a fleet nobody checked is the
    // answer this table exists to not give.
    if agg.unchecked > 0 {
        col = col.push(muted_owned(format!(
            "{} container(s) were not checked against their upstream digest — that is \
             not the same as up to date. Turn on the egressing image collector to \
             include them.",
            agg.unchecked
        )));
    }

    col = col.push(text("Patch drift").size(font::EMPHASIS));
    let drifted: Vec<&ContainerRow> = rows
        .iter()
        .filter(|r| r.image == ImageStanding::Behind)
        .collect();
    if drifted.is_empty() {
        col = col.push(muted(if agg.unchecked == agg.total {
            "Nothing checked — no image drift can be reported"
        } else {
            "No checked container is behind its upstream digest"
        }));
    } else {
        for r in drifted.iter().take(20) {
            col = col.push(
                row![
                    text(r.label.clone()).size(font::DENSE),
                    text("behind upstream")
                        .size(font::DENSE)
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).warning()),
                        }),
                ]
                .spacing(space::MD)
                .align_y(Alignment::Center),
            );
        }
    }

    let troubled: Vec<&ContainerRow> = rows
        .iter()
        .filter(|r| r.oom_kills > 0 || r.restarts > 0)
        .collect();
    if !troubled.is_empty() {
        col = col.push(text("OOM kills & restarts").size(font::EMPHASIS));
        for r in troubled.iter().take(20) {
            let psi = match r.worst_pressure {
                Some((what, v)) => format!("{what} PSI {v:.1}"),
                None => String::new(),
            };
            col = col.push(
                row![
                    text(r.label.clone()).size(font::DENSE),
                    text(format!("{} OOM", r.oom_kills)).size(font::DENSE),
                    text(format!("{} restarts", r.restarts)).size(font::DENSE),
                    text(psi).size(font::MICRO),
                ]
                .spacing(space::MD)
                .align_y(Alignment::Center),
            );
        }
    }

    col.into()
}

fn muted<'a>(s: &'a str) -> Element<'a, Message> {
    text(s)
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn muted_owned<'a>(s: String) -> Element<'a, Message> {
    text(s)
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label)
            .size(font::MICRO)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        text(value).size(font::EMPHASIS)
    ]
    .spacing(space::XS)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::{Protocol, TelemetryPoint};

    fn dev(host: &str, metrics: &[(&str, f64)]) -> (DeviceId, DeviceState) {
        let id = DeviceId::fixture(Protocol::Container, host);
        let mut state = DeviceState::new(id.clone());
        for (metric, v) in metrics {
            state.metrics.insert(
                (*metric).to_string(),
                TelemetryPoint::new(
                    host,
                    Protocol::Container,
                    (*metric).to_string(),
                    TelemetryValue::Gauge(*v),
                ),
            );
        }
        (id, state)
    }

    fn fleet(pairs: &[(DeviceId, DeviceState)]) -> HashMap<&DeviceId, &DeviceState> {
        pairs.iter().map(|(i, s)| (i, s)).collect()
    }

    /// The rule this table exists to hold. `image_behind_upstream` is published
    /// only when the digest was actually resolved
    /// (`zensight-sensor-container/src/poller.rs:322`), so its absence means
    /// nobody looked. Counting `nginx` as current here would answer "is my
    /// fleet patched?" with a confident yes about a container nothing checked.
    #[test]
    fn an_unchecked_image_is_never_counted_as_current() {
        let pairs = [dev(
            "host01",
            &[
                ("redis/image_behind_upstream", 1.0),
                ("caddy/image_behind_upstream", 0.0),
                // No `image_behind_upstream` — the collector is off for it.
                ("nginx/memory_bytes", 1.0e8),
            ],
        )];
        let rows = container_rows(&fleet(&pairs));
        let agg = aggregate(&rows);

        assert_eq!(agg.total, 3);
        assert_eq!(agg.behind, 1);
        assert_eq!(agg.unchecked, 1, "nginx is unchecked, not current");

        let nginx = rows.iter().find(|r| r.label == "host01/nginx").unwrap();
        assert_eq!(nginx.image, ImageStanding::Unchecked);
        assert_ne!(nginx.image, ImageStanding::Current);
    }

    /// A container name is unique on its host and nowhere else. Two hosts each
    /// running `redis` are two containers, and folding them on the bare name
    /// would report one host's OOM kills against the other's.
    #[test]
    fn two_hosts_running_the_same_image_are_two_containers() {
        let pairs = [
            dev("host01", &[("redis/oom_kills_total", 3.0)]),
            dev("host02", &[("redis/oom_kills_total", 0.0)]),
        ];
        let rows = container_rows(&fleet(&pairs));
        assert_eq!(rows.len(), 2);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"host01/redis"));
        assert!(labels.contains(&"host02/redis"));
        let one = rows.iter().find(|r| r.label == "host01/redis").unwrap();
        assert_eq!(one.oom_kills, 3);
    }

    /// Drifted first, then most-OOM-killed — the triage order.
    #[test]
    fn drift_outranks_oom_kills_in_the_sort() {
        let pairs = [dev(
            "host01",
            &[
                ("a/oom_kills_total", 99.0),
                ("a/image_behind_upstream", 0.0),
                ("b/image_behind_upstream", 1.0),
            ],
        )];
        let rows = container_rows(&fleet(&pairs));
        assert_eq!(rows[0].label, "host01/b", "drift first");
        assert_eq!(rows[1].label, "host01/a");
    }

    /// The worst of the three PSI gauges is the one shown, and it is named —
    /// "PSI 40" without saying *which* pressure sends you to the wrong place.
    #[test]
    fn the_worst_pressure_gauge_is_the_one_reported_and_it_is_named() {
        let pairs = [dev(
            "host01",
            &[
                ("db/cpu_pressure_avg10", 2.0),
                ("db/memory_pressure_avg10", 41.5),
                ("db/io_pressure_avg10", 8.0),
            ],
        )];
        let rows = container_rows(&fleet(&pairs));
        assert_eq!(rows[0].worst_pressure, Some(("memory", 41.5)));
    }

    /// A fleet nobody checked reports that, rather than "no drift".
    #[test]
    fn a_wholly_unchecked_fleet_does_not_report_a_clean_bill() {
        let pairs = [dev("host01", &[("redis/memory_bytes", 1.0)])];
        let rows = container_rows(&fleet(&pairs));
        let agg = aggregate(&rows);
        assert_eq!(agg.behind, 0);
        assert_eq!(agg.unchecked, agg.total);
    }
}
