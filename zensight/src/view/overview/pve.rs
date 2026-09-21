//! Proxmox VE overview — backup freshness, cluster quorum, storage overcommit.
//!
//! #1128: `cluster/quorate`, `cluster/replication_failed`,
//! `backup/{vmid}/age_secs` + `ok`, and `storage/{s}/overcommit_ratio` have
//! been on the bus since #818 and reached a generic key/value row on a device
//! card. The question every hypervisor operator asks first — *which guests are
//! not backed up* — had no answer anywhere in the GUI.
//!
//! Two absences carry the weight here, and both are easy to render as their
//! opposite:
//!
//! - **A guest with no backup subject at all has never been backed up.** That
//!   is the worst row in the table, not a missing one. A table built by walking
//!   `backup/{vmid}/*` would omit exactly the guests the table exists to find,
//!   so this one walks the **guests** and left-joins their backups.
//! - **Without quorum, the cluster's own numbers are a minority report.** A
//!   non-quorate node still answers `/cluster/resources` and still reports
//!   guests as running. Saying so above the tables is the difference between
//!   reading them and believing them.

use std::collections::{BTreeMap, HashMap};

use iced::widget::{Column, column, row, text};
use iced::{Alignment, Element, Theme};

use zensight_common::TelemetryValue;
use zensight_common::registry::pve::Subject;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// How a guest's last backup stands. Ordered worst-first, which is the order
/// the table sorts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackupStanding {
    /// No `backup/{vmid}/*` subject exists for a guest that does.
    Never,
    /// The last run reported `ok = 0`.
    Failed,
    /// Older than [`STALE_AFTER_SECS`].
    Stale,
    /// Ran, succeeded, recent.
    Fresh,
}

/// A backup older than this is called out. Proxmox's own default schedule is
/// daily, so two days is one missed run plus slack for a long-running job.
pub const STALE_AFTER_SECS: f64 = 172_800.0;

/// One row of the backup-freshness table.
#[derive(Debug, Clone)]
pub struct BackupRow {
    pub vmid: String,
    pub standing: BackupStanding,
    pub age_secs: Option<f64>,
    pub size_change_pct: Option<f64>,
    pub running: bool,
}

/// What this fleet's pve sensors say about the cluster.
#[derive(Debug, Default, Clone)]
pub struct PveAgg {
    pub nodes_online: u64,
    pub nodes_total: u64,
    pub guests_running: u64,
    pub guests_total: u64,
    /// `None` when no sensor published `cluster/quorate` — a single node with
    /// no cluster configured, where quorum is not a question.
    pub quorate: Option<bool>,
    pub replication_failed: u64,
    /// `store -> overcommit ratio`, only those over 1.0.
    pub overcommitted: BTreeMap<String, f64>,
}

fn num(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(c) => Some(*c as f64),
        TelemetryValue::Gauge(g) => Some(*g),
        TelemetryValue::Boolean(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// Fold every pve device's metrics into the cluster summary.
#[must_use]
pub fn aggregate(devices: &HashMap<&DeviceId, &DeviceState>) -> PveAgg {
    let mut agg = PveAgg::default();
    for state in devices.values() {
        for (key, point) in &state.metrics {
            let Some(subject) = Subject::parse_metric(key) else {
                continue;
            };
            let Some(v) = num(&point.value) else { continue };
            match subject {
                Subject::ClusterNodesOnline => agg.nodes_online = agg.nodes_online.max(v as u64),
                Subject::ClusterNodesTotal => agg.nodes_total = agg.nodes_total.max(v as u64),
                Subject::ClusterGuestsRunning => {
                    agg.guests_running = agg.guests_running.max(v as u64);
                }
                Subject::ClusterGuestsTotal => agg.guests_total = agg.guests_total.max(v as u64),
                // Any sensor reporting a loss of quorum wins over one
                // reporting it intact: a split cluster has a majority side
                // that still says `quorate = 1`, and that half is not the
                // half worth hearing.
                Subject::ClusterQuorate => {
                    agg.quorate = Some(agg.quorate.unwrap_or(true) && v != 0.0);
                }
                Subject::ClusterReplicationFailed => {
                    agg.replication_failed = agg.replication_failed.max(v as u64);
                }
                Subject::StorageOvercommitRatio { store } if v > 1.0 => {
                    agg.overcommitted.insert(store.to_string(), v);
                }
                _ => {}
            }
        }
    }
    agg
}

/// Build the backup-freshness table, worst first.
///
/// Walks the **guests** and joins their backups, never the other way round —
/// see the module note.
#[must_use]
pub fn backup_rows(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<BackupRow> {
    #[derive(Default)]
    struct Raw {
        running: bool,
        age_secs: Option<f64>,
        ok: Option<bool>,
        size_change_pct: Option<f64>,
    }

    let mut guests: BTreeMap<String, Raw> = BTreeMap::new();

    for state in devices.values() {
        for (key, point) in &state.metrics {
            let Some(subject) = Subject::parse_metric(key) else {
                continue;
            };
            let v = num(&point.value);
            match subject {
                Subject::GuestRunning { vmid } => {
                    guests.entry(vmid.to_string()).or_default().running =
                        v.is_some_and(|n| n != 0.0);
                }
                Subject::BackupAgeSecs { vmid } => {
                    guests.entry(vmid.to_string()).or_default().age_secs = v;
                }
                Subject::BackupOk { vmid } => {
                    guests.entry(vmid.to_string()).or_default().ok = v.map(|n| n != 0.0);
                }
                Subject::BackupSizeChangePct { vmid } => {
                    guests.entry(vmid.to_string()).or_default().size_change_pct = v;
                }
                // A guest that exists at all — any `guest/{vmid}/*` subject —
                // earns a row. A stopped guest publishes no cpu or memory but
                // is still a guest whose disk wants backing up.
                Subject::GuestCpuRatio { vmid }
                | Subject::GuestMemBytes { vmid }
                | Subject::GuestDiskBytes { vmid }
                | Subject::GuestUptimeSecs { vmid } => {
                    guests.entry(vmid.to_string()).or_default();
                }
                _ => {}
            }
        }
    }

    let mut rows: Vec<BackupRow> = guests
        .into_iter()
        .map(|(vmid, r)| {
            let standing = match (r.age_secs, r.ok) {
                (_, Some(false)) => BackupStanding::Failed,
                (None, _) => BackupStanding::Never,
                (Some(a), _) if a > STALE_AFTER_SECS => BackupStanding::Stale,
                _ => BackupStanding::Fresh,
            };
            BackupRow {
                vmid,
                standing,
                age_secs: r.age_secs,
                size_change_pct: r.size_change_pct,
                running: r.running,
            }
        })
        .collect();

    // Worst first, then oldest first within a standing, then by vmid so the
    // order is stable across polls.
    rows.sort_by(|a, b| {
        a.standing.cmp(&b.standing).then_with(|| {
            b.age_secs
                .unwrap_or(f64::MAX)
                .total_cmp(&a.age_secs.unwrap_or(f64::MAX))
                .then_with(|| a.vmid.cmp(&b.vmid))
        })
    });
    rows
}

/// Human-readable age. Backups are a daily thing, so days are the unit that
/// carries the meaning; anything under a day is "today".
fn age_label(row: &BackupRow) -> String {
    match row.age_secs {
        None => "never".to_string(),
        Some(a) if a < 86_400.0 => format!("{:.0}h", a / 3600.0),
        Some(a) => format!("{:.1}d", a / 86_400.0),
    }
}

fn standing_label(s: BackupStanding) -> &'static str {
    match s {
        BackupStanding::Never => "never backed up",
        BackupStanding::Failed => "last run failed",
        BackupStanding::Stale => "stale",
        BackupStanding::Fresh => "ok",
    }
}

/// Render the pve overview.
pub fn pve_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return muted("No Proxmox nodes available");
    }

    let agg = aggregate(devices);
    let rows = backup_rows(devices);

    let mut col = Column::new().spacing(space::SM);

    let summary = row![
        stat("Nodes", format!("{}/{}", agg.nodes_online, agg.nodes_total)),
        stat(
            "Guests running",
            format!("{}/{}", agg.guests_running, agg.guests_total)
        ),
        status_stat(
            "Replication failed",
            agg.replication_failed as usize,
            if agg.replication_failed == 0 {
                StatusLedState::Active
            } else {
                StatusLedState::Inactive
            }
        ),
    ]
    .spacing(space::LG)
    .align_y(Alignment::Center);
    col = col.push(summary);

    // Quorum above everything it casts doubt on.
    match agg.quorate {
        Some(false) => {
            col = col.push(
                text(
                    "The cluster is NOT quorate — a node in the minority still answers, \
                     and still reports guests as running. Read everything below as one \
                     partition's view.",
                )
                .size(font::CAPTION)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).danger()),
                }),
            );
        }
        Some(true) => {
            col = col.push(muted("Cluster quorate"));
        }
        // Not "quorate": nobody said so. A standalone node has no cluster.
        None => {
            col = col.push(muted("No cluster quorum reported — standalone node(s)"));
        }
    }

    col = col.push(text("Backup freshness").size(font::EMPHASIS));
    if rows.is_empty() {
        col = col.push(muted("No guests reported"));
    } else {
        for r in rows.iter().take(20) {
            let stale = r.standing != BackupStanding::Fresh;
            let standing =
                text(standing_label(r.standing))
                    .size(font::DENSE)
                    .style(move |t: &Theme| text::Style {
                        color: Some(if stale {
                            theme::colors(t).danger()
                        } else {
                            theme::colors(t).text_muted()
                        }),
                    });
            let trend = match r.size_change_pct {
                Some(p) => format!("{p:+.1}%"),
                None => String::new(),
            };
            col = col.push(
                row![
                    text(format!("vmid {}", r.vmid)).size(font::DENSE),
                    text(if r.running { "running" } else { "stopped" }).size(font::MICRO),
                    text(age_label(r)).size(font::DENSE),
                    standing,
                    text(trend).size(font::MICRO),
                ]
                .spacing(space::MD)
                .align_y(Alignment::Center),
            );
        }
        if rows.len() > 20 {
            col = col.push(muted_owned(format!(
                "… and {} more guests",
                rows.len() - 20
            )));
        }
    }

    if !agg.overcommitted.is_empty() {
        col = col.push(text("Storage overcommitted").size(font::EMPHASIS));
        for (store, ratio) in &agg.overcommitted {
            col = col.push(
                row![
                    text(store.clone()).size(font::DENSE),
                    text(format!("{ratio:.2}× allocated vs capacity")).size(font::DENSE),
                ]
                .spacing(space::MD),
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

fn status_stat<'a>(label: &'a str, count: usize, state: StatusLedState) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);
    column![
        text(label)
            .size(font::MICRO)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        row![led.view(), text(count.to_string()).size(font::EMPHASIS)]
            .spacing(space::XS)
            .align_y(Alignment::Center)
    ]
    .spacing(space::XS)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::TelemetryPoint;

    fn dev(metrics: &[(&str, TelemetryValue)]) -> (DeviceId, DeviceState) {
        let id = DeviceId::fixture("pve", "pve01");
        let mut state = DeviceState::new(id.clone());
        for (metric, v) in metrics {
            state.metrics.insert(
                (*metric).to_string(),
                TelemetryPoint::new("pve01", (*metric).to_string(), v.clone()),
            );
        }
        (id, state)
    }

    fn fleet(pairs: &[(DeviceId, DeviceState)]) -> HashMap<&DeviceId, &DeviceState> {
        pairs.iter().map(|(i, s)| (i, s)).collect()
    }

    /// The whole reason the table walks guests and not backups.
    ///
    /// vmid 101 has a fresh backup; vmid 102 has no `backup/*` subject at all.
    /// A table built from the backup subjects would contain exactly one row —
    /// the guest that is fine — and would have silently dropped the only row
    /// anyone opens this table to find.
    #[test]
    fn a_guest_with_no_backup_subject_is_the_top_row_not_a_missing_one() {
        let pairs = [dev(&[
            ("guest/101/running", TelemetryValue::Gauge(1.0)),
            ("backup/101/age_secs", TelemetryValue::Gauge(3600.0)),
            ("backup/101/ok", TelemetryValue::Gauge(1.0)),
            ("guest/102/running", TelemetryValue::Gauge(1.0)),
        ])];
        let rows = backup_rows(&fleet(&pairs));

        assert_eq!(rows.len(), 2, "both guests get a row");
        assert_eq!(rows[0].vmid, "102");
        assert_eq!(rows[0].standing, BackupStanding::Never);
        assert_eq!(rows[0].age_secs, None);
        assert_eq!(rows[1].vmid, "101");
        assert_eq!(rows[1].standing, BackupStanding::Fresh);
    }

    /// A stopped guest still has a disk, and that disk still wants backing up.
    /// It publishes no cpu or uptime, so a row keyed on liveness would lose it.
    /// #1257: `backup_rows` joins two families the slice declares
    /// separately — `guest/{vmid}` and `backup/{vmid}` share a variable, not
    /// a prefix — by their binding. The family model derives both; the join
    /// is the view definition's (§6.3). So the union of the two families'
    /// instances is exactly the row set, and each row's numbers are the
    /// instance's fields.
    #[test]
    fn the_family_model_reproduces_the_hand_written_join() {
        use crate::view::family::FamilyModel;
        use std::collections::BTreeSet;
        let pairs = [dev(&[
            ("guest/101/running", TelemetryValue::Gauge(1.0)),
            ("backup/101/age_secs", TelemetryValue::Gauge(3600.0)),
            ("backup/101/ok", TelemetryValue::Gauge(1.0)),
            ("backup/101/size_change_pct", TelemetryValue::Gauge(2.5)),
            ("guest/102/running", TelemetryValue::Gauge(1.0)),
            ("guest/103/disk_bytes", TelemetryValue::Gauge(4.2e10)),
            ("backup/104/age_secs", TelemetryValue::Gauge(7.0)),
        ])];
        let rows = backup_rows(&fleet(&pairs));
        let model = FamilyModel::for_producer("pve").expect("pve is compiled in");
        let derived = model.instances(pairs[0].1.metrics.iter());
        let instances_of = |path: &str| -> Vec<&crate::view::family::Instance> {
            let idx = model.families.iter().position(|f| f.path == path).unwrap();
            derived
                .iter()
                .find(|f| f.family == idx)
                .map(|f| f.instances.iter().collect())
                .unwrap_or_default()
        };
        let guests = instances_of("guest/{vmid}");
        let backups = instances_of("backup/{vmid}");
        let vmids: BTreeSet<&str> = guests
            .iter()
            .chain(backups.iter())
            .map(|i| i.id.as_str())
            .collect();
        let row_vmids: BTreeSet<&str> = rows.iter().map(|r| r.vmid.as_str()).collect();
        assert_eq!(
            vmids, row_vmids,
            "the join's row set is the two families' union"
        );
        for row in &rows {
            let backup = backups.iter().find(|i| i.id == row.vmid);
            let guest = guests.iter().find(|i| i.id == row.vmid);
            assert_eq!(row.age_secs, backup.and_then(|b| b.number("age_secs")));
            assert_eq!(
                row.size_change_pct,
                backup.and_then(|b| b.number("size_change_pct"))
            );
            assert_eq!(
                row.running,
                guest.and_then(|g| g.state("running")).unwrap_or(false)
            );
        }
    }

    #[test]
    fn a_stopped_guest_still_earns_a_backup_row() {
        let pairs = [dev(&[(
            "guest/103/disk_bytes",
            TelemetryValue::Gauge(4.2e10),
        )])];
        let rows = backup_rows(&fleet(&pairs));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vmid, "103");
        assert!(!rows[0].running);
        assert_eq!(rows[0].standing, BackupStanding::Never);
    }

    /// A recent backup that *failed* is worse than an old one that worked, and
    /// the age alone cannot say so.
    #[test]
    fn a_fresh_failed_backup_outranks_a_merely_stale_one() {
        let pairs = [dev(&[
            ("guest/201/running", TelemetryValue::Gauge(1.0)),
            ("backup/201/age_secs", TelemetryValue::Gauge(60.0)),
            ("backup/201/ok", TelemetryValue::Gauge(0.0)),
            ("guest/202/running", TelemetryValue::Gauge(1.0)),
            ("backup/202/age_secs", TelemetryValue::Gauge(500_000.0)),
            ("backup/202/ok", TelemetryValue::Gauge(1.0)),
        ])];
        let rows = backup_rows(&fleet(&pairs));
        assert_eq!(rows[0].vmid, "201");
        assert_eq!(rows[0].standing, BackupStanding::Failed);
        assert_eq!(rows[1].standing, BackupStanding::Stale);
    }

    /// A split cluster has a majority side still reporting `quorate = 1`. The
    /// fleet reading has to be the pessimistic one, or the partition is
    /// invisible from exactly the half that can still see the GUI.
    #[test]
    fn one_node_reporting_lost_quorum_outweighs_the_others() {
        let mut a = dev(&[("cluster/quorate", TelemetryValue::Gauge(1.0))]);
        a.0 = DeviceId::fixture("pve", "pve-a");
        let mut b = dev(&[("cluster/quorate", TelemetryValue::Gauge(0.0))]);
        b.0 = DeviceId::fixture("pve", "pve-b");
        let pairs = [a, b];
        assert_eq!(aggregate(&fleet(&pairs)).quorate, Some(false));
    }

    /// No `cluster/quorate` anywhere is a standalone node, not a lost quorum —
    /// and not a quorate cluster either.
    #[test]
    fn a_standalone_node_reports_no_quorum_rather_than_a_verdict() {
        let pairs = [dev(&[("guest/101/running", TelemetryValue::Gauge(1.0))])];
        assert_eq!(aggregate(&fleet(&pairs)).quorate, None);
    }

    /// Only over-committed pools are listed; a pool allocated under its
    /// capacity is not a finding.
    #[test]
    fn only_overcommitted_storage_is_listed() {
        let pairs = [dev(&[
            ("storage/local/overcommit_ratio", TelemetryValue::Gauge(0.8)),
            ("storage/ceph/overcommit_ratio", TelemetryValue::Gauge(2.4)),
        ])];
        let agg = aggregate(&fleet(&pairs));
        assert_eq!(agg.overcommitted.len(), 1);
        assert_eq!(agg.overcommitted.get("ceph"), Some(&2.4));
    }
}
