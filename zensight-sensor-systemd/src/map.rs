//! Pure per-unit telemetry mapping (#273): [`UnitSample`] → [`TelemetryPoint`]s
//! under `systemd/unit/<unit>/*`, plus the `systemd/other/*` overflow bucket.
//! Kept free of I/O so it is unit-testable without a bus.

use zensight_common::registry::systemd::Subject;
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

use crate::unit::UnitSample;

/// One built point beside the subject it publishes under (#1274): the
/// collector hands both to `Publisher::publish_subject`, and the metric on
/// the point is the subject's tail by construction.
pub type Built = (Subject, TelemetryPoint);

/// A point under `subject` from `source`, paired with it.
fn built(source: &str, subject: Subject, value: TelemetryValue) -> Built {
    let point = TelemetryPoint::for_subject(source, &subject, value);
    (subject, point)
}

/// Slug a unit name into a legal key-expression chunk (#843).
///
/// A unit name is foreign data, and the boundary where foreign data becomes
/// grammar-legal is `zensight_sensor_core::key::device_chunk` — `Chunk::slug`,
/// RFC 03 §2's injective escape, and nothing else (#1153) — not a hand-rolled
/// character map. The hand-rolled one this replaces had
/// both defects the RFC warns about: it never folded case, so any unit with
/// an uppercase letter (`NetworkManager.service` — much of a stock host)
/// produced a chunk the grammar refuses, panicking the collector in debug
/// builds and publishing unregistered keys in release; and its `→ _`
/// substitution was not injective (`user@1000.service` and
/// `user_1000.service` shared a chunk).
///
/// Already-legal names (`sshd.service`) stay byte-identical, and the raw
/// name always rides the point's `unit` label, so nothing readable is lost.
pub fn sanitize_unit(name: &str) -> String {
    zensight_sensor_core::key::device_chunk(name)
        .as_str()
        .to_string()
}

/// Build the per-unit telemetry points for one watched unit. Every point carries
/// a `unit` label with the raw name; resource points are emitted only when the
/// unit has the matching accounting enabled (`Some`), and `exit_code` only when
/// the unit is failed.
pub fn unit_points(source: &str, s: &UnitSample) -> Vec<Built> {
    // The builders slug the unit name themselves (#1274) — the same escape
    // `sanitize_unit` applies for the keys that are not telemetry.
    let unit = s.name.as_str();
    let point = |subject: Subject, value: TelemetryValue| {
        let (subject, point) = built(source, subject, value);
        (subject, point.with_label("unit", s.name.clone()))
    };

    let (state_subject, state_point) = built(
        source,
        Subject::unit_state(unit),
        TelemetryValue::Text(s.active_state.clone()),
    );
    let mut pts = vec![
        point(
            Subject::unit_active(unit),
            TelemetryValue::Boolean(s.is_active()),
        ),
        // Active/sub state as text; load_state rides as a label for context.
        (
            state_subject,
            state_point
                .with_label("unit", s.name.clone())
                .with_label("load_state", s.load_state.clone())
                .with_label("sub_state", s.sub_state.clone()),
        ),
        point(
            Subject::unit_restarts_total(unit),
            TelemetryValue::Counter(s.n_restarts as u64),
        ),
        point(
            Subject::unit_active_since_usec(unit),
            TelemetryValue::Gauge(s.active_enter_usec as f64),
        ),
    ];

    if let Some(mem) = s.mem_bytes {
        pts.push(point(
            Subject::unit_mem_bytes(unit),
            TelemetryValue::Gauge(mem as f64),
        ));
    }
    if let Some(cpu) = s.cpu_usec {
        // CPU time is monotonic → Counter.
        pts.push(point(
            Subject::unit_cpu_usec(unit),
            TelemetryValue::Counter(cpu),
        ));
    }
    if let Some(tasks) = s.tasks {
        pts.push(point(
            Subject::unit_tasks(unit),
            TelemetryValue::Gauge(tasks as f64),
        ));
    }
    // Exit code is only meaningful for a failed unit.
    if s.is_failed() {
        pts.push(point(
            Subject::unit_exit_code(unit),
            TelemetryValue::Gauge(s.exec_main_status as f64),
        ));
    }
    // Opt-in IP/IO accounting (present only when the unit enabled it).
    for (subject, val) in [
        (Subject::unit_ip_ingress_bytes(unit), s.ip_ingress_bytes),
        (Subject::unit_ip_egress_bytes(unit), s.ip_egress_bytes),
        (Subject::unit_io_read_bytes(unit), s.io_read_bytes),
        (Subject::unit_io_write_bytes(unit), s.io_write_bytes),
    ] {
        if let Some(v) = val {
            pts.push(point(subject, TelemetryValue::Counter(v)));
        }
    }
    pts
}

/// Per-unit IP bandwidth-rate points (#315): `unit/<name>/{ip_ingress_bps,
/// ip_egress_bps}` as **wire-L3** gauges (cgroup_skb: L3+ bytes incl. retransmits,
/// no L2 framing), labelled `bw.source=systemd`/`bw.semantics=wire-l3` so the GUI
/// never blends them with app-goodput (sock_diag/eBPF) or wire-L2 (capture)
/// sources. When `accounting_off` (an *active* unit with IPAccounting disabled),
/// emit `unit/<name>/ip_accounting=false` instead of a silent zero so the GUI can
/// show the "off" state distinctly.
pub fn ip_rate_points(
    source: &str,
    unit: &str,
    ingress_bps: Option<f64>,
    egress_bps: Option<f64>,
    accounting_off: bool,
) -> Vec<Built> {
    use zensight_common::bandwidth::{
        BandwidthSource, ByteSemantics, LABEL_SEMANTICS, LABEL_SOURCE,
    };
    let gauge = |subject: Subject, v: f64| {
        let (subject, point) = built(source, subject, TelemetryValue::Gauge(v));
        (
            subject,
            point
                .with_label("unit", unit.to_string())
                .with_label(LABEL_SOURCE, BandwidthSource::Systemd.as_str())
                .with_label(LABEL_SEMANTICS, ByteSemantics::WireL3.as_str())
                .with_label("accounting", "cgroup_skb"),
        )
    };
    let mut pts = Vec::new();
    if let Some(bps) = ingress_bps {
        pts.push(gauge(Subject::unit_ip_ingress_bps(unit), bps));
    }
    if let Some(bps) = egress_bps {
        pts.push(gauge(Subject::unit_ip_egress_bps(unit), bps));
    }
    if accounting_off {
        let (subject, point) = built(
            source,
            Subject::unit_ip_accounting(unit),
            TelemetryValue::Boolean(false),
        );
        pts.push((subject, point.with_label("unit", unit.to_string())));
    }
    pts
}

/// Per-socket-unit counters (#279): `unit/<socket>/{n_accepted,n_connections,
/// n_refused}`. Emitted for watched `.socket` units.
pub fn socket_points(
    source: &str,
    name: &str,
    n_accepted: u32,
    n_connections: u32,
    n_refused: u32,
) -> Vec<Built> {
    let point = |subject: Subject, value: TelemetryValue| {
        let (subject, point) = built(source, subject, value);
        (subject, point.with_label("unit", name))
    };
    vec![
        // n_accepted is monotonic (lifetime connections accepted) → Counter.
        point(
            Subject::unit_n_accepted(name),
            TelemetryValue::Counter(n_accepted as u64),
        ),
        point(
            Subject::unit_n_connections(name),
            TelemetryValue::Gauge(n_connections as f64),
        ),
        point(
            Subject::unit_n_refused(name),
            TelemetryValue::Counter(n_refused as u64),
        ),
    ]
}

/// Per-timer-unit schedule (#279): `unit/<timer>/{last_trigger_usec,
/// next_trigger_usec}`. Emitted for watched `.timer` units. `u64::MAX` next-elapse
/// (no scheduled run) is dropped.
pub fn timer_points(
    source: &str,
    name: &str,
    last_trigger_usec: u64,
    next_elapse_usec: u64,
) -> Vec<Built> {
    let point = |subject: Subject, value: TelemetryValue| {
        let (subject, point) = built(source, subject, value);
        (subject, point.with_label("unit", name))
    };
    let mut pts = vec![point(
        Subject::unit_last_trigger_usec(name),
        TelemetryValue::Gauge(last_trigger_usec as f64),
    )];
    if next_elapse_usec != 0 && next_elapse_usec != u64::MAX {
        pts.push(point(
            Subject::unit_next_trigger_usec(name),
            TelemetryValue::Gauge(next_elapse_usec as f64),
        ));
    }
    pts
}

/// Mount/automount state aggregates (#279, `collect.mounts`): `mounts/{total,
/// mounted,failed}` from the enumerated units. `states` is the `active_state` of
/// each `.mount`/`.automount` unit.
pub fn mount_points<'a>(source: &str, states: impl IntoIterator<Item = &'a str>) -> Vec<Built> {
    let (mut total, mut mounted, mut failed) = (0u64, 0u64, 0u64);
    for s in states {
        total += 1;
        match s {
            "active" | "mounted" => mounted += 1,
            "failed" => failed += 1,
            _ => {}
        }
    }
    let gauge = |subject: Subject, v: u64| built(source, subject, TelemetryValue::Gauge(v as f64));
    vec![
        gauge(Subject::MountsTotal, total),
        gauge(Subject::MountsMounted, mounted),
        gauge(Subject::MountsFailed, failed),
    ]
}

/// Journal store health (#279, `collect.journal`): `journal/{disk_usage_bytes,
/// disk_available_bytes}`.
pub fn journal_points(source: &str, usage_bytes: u64, available_bytes: Option<u64>) -> Vec<Built> {
    let gauge = |subject: Subject, v: f64| built(source, subject, TelemetryValue::Gauge(v));
    let mut pts = vec![gauge(Subject::JournalDiskUsageBytes, usage_bytes as f64)];
    if let Some(avail) = available_bytes {
        pts.push(gauge(Subject::JournalDiskAvailableBytes, avail as f64));
    }
    pts
}

/// The `systemd/other/*` overflow bucket (#273): a single gauge counting the
/// units that are NOT individually streamed (total minus watched), so their
/// existence isn't lost to the watchlist scoping.
pub fn other_points(source: &str, unwatched_total: u64) -> Vec<Built> {
    vec![built(
        source,
        Subject::OtherUnitsTotal,
        TelemetryValue::Gauge(unwatched_total as f64),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> UnitSample {
        UnitSample {
            name: name.into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            active_enter_usec: 1_700_000_000_000,
            n_restarts: 3,
            mem_bytes: Some(4096),
            cpu_usec: Some(500),
            tasks: Some(7),
            exec_main_status: 0,
            ..Default::default()
        }
    }

    /// The escaped outputs, pinned. This is the only place in ZenSight that
    /// slugs an operator-visible name into a key chunk on a live host, so a
    /// silent upstream change to the escape re-keys a whole fleet's
    /// `systemd/unit/*` series and nothing else would notice. zenkey 0.8 did
    /// exactly that (its #418: the v1.4 escape was not injective either way
    /// round), and this table is what turns the next one into a failing test
    /// rather than a fleet-wide gap in the history.
    #[test]
    fn sanitize_outputs_are_pinned() {
        for (name, chunk) in [
            // Already-legal names stay byte-identical, on every version.
            ("sshd.service", "sshd.service"),
            ("user_1000.service", "user_1000.service"),
            // Foreign bytes get the injective escape — `x-` reserved on both
            // sides of the boundary, `_xHH` with no closing underscore
            // (RFC 03 §2 v1.31).
            ("user@1000.service", "x-user_x401000.service"),
            ("NetworkManager.service", "x-_x4eetwork_x4danager.service"),
        ] {
            assert_eq!(sanitize_unit(name), chunk, "{name}");
        }
        // The property the escape exists for (#843): a name with a foreign
        // byte can never collide with a literal that spells it out.
        assert_ne!(
            sanitize_unit("user@1000.service"),
            sanitize_unit("user_1000.service")
        );
        // The bug that found this (#843): uppercase unit names are real
        // (NetworkManager.service) and must yield a legal chunk, not a
        // debug-build panic.
        for name in ["NetworkManager.service", "ModemManager.service", "a b/c"] {
            let chunk = sanitize_unit(name);
            assert!(
                zensight_common::registry::is_registered_telemetry(
                    "systemd",
                    &format!("unit/{chunk}/active")
                ),
                "{name:?} -> {chunk:?} does not satisfy the registered pattern"
            );
        }
    }

    #[test]
    fn active_unit_points_shape_and_labels() {
        let pts = unit_points("host01", &sample("nginx.service"));
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|(_, p)| (p.metric.as_str(), p)).collect();
        assert_eq!(
            by["unit/nginx.service/active"].value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            by["unit/nginx.service/restarts_total"].value,
            TelemetryValue::Counter(3)
        );
        assert_eq!(
            by["unit/nginx.service/mem_bytes"].value,
            TelemetryValue::Gauge(4096.0)
        );
        assert_eq!(
            by["unit/nginx.service/cpu_usec"].value,
            TelemetryValue::Counter(500)
        );
        // Every point carries the raw unit name as a label.
        assert_eq!(
            by["unit/nginx.service/active"]
                .labels
                .get("unit")
                .map(String::as_str),
            Some("nginx.service")
        );
        // state carries load/sub state labels.
        let state = by["unit/nginx.service/state"];
        assert_eq!(state.value, TelemetryValue::Text("active".into()));
        assert_eq!(
            state.labels.get("load_state").map(String::as_str),
            Some("loaded")
        );
        // Not failed → no exit_code point.
        assert!(!by.contains_key("unit/nginx.service/exit_code"));
    }

    #[test]
    fn failed_unit_emits_exit_code_absent_accounting_omitted() {
        let mut s = sample("bad.service");
        s.active_state = "failed".into();
        s.exec_main_status = 203;
        s.mem_bytes = None; // accounting disabled
        s.cpu_usec = None;
        s.tasks = None;
        let pts = unit_points("host01", &s);
        let by: std::collections::HashMap<_, _> = pts
            .iter()
            .map(|(_, p)| (p.metric.as_str(), &p.value))
            .collect();
        assert_eq!(
            by["unit/bad.service/exit_code"],
            &TelemetryValue::Gauge(203.0)
        );
        assert_eq!(
            by["unit/bad.service/active"],
            &TelemetryValue::Boolean(false)
        );
        assert!(!by.contains_key("unit/bad.service/mem_bytes"));
        assert!(!by.contains_key("unit/bad.service/tasks"));
    }

    #[test]
    fn other_bucket_is_single_gauge() {
        let pts = other_points("host01", 512);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].1.metric, "other/units_total");
        assert_eq!(pts[0].1.value, TelemetryValue::Gauge(512.0));
    }

    #[test]
    fn socket_points_shape() {
        let pts = socket_points("h", "sshd.socket", 12, 3, 1);
        let by: std::collections::HashMap<_, _> = pts
            .iter()
            .map(|(_, p)| (p.metric.as_str(), &p.value))
            .collect();
        assert_eq!(
            by["unit/sshd.socket/n_accepted"],
            &TelemetryValue::Counter(12)
        );
        assert_eq!(
            by["unit/sshd.socket/n_connections"],
            &TelemetryValue::Gauge(3.0)
        );
        assert_eq!(
            by["unit/sshd.socket/n_refused"],
            &TelemetryValue::Counter(1)
        );
        assert_eq!(
            pts[0].1.labels.get("unit").map(String::as_str),
            Some("sshd.socket")
        );
    }

    #[test]
    fn timer_points_drops_absent_next() {
        // Scheduled next → both points.
        let pts = timer_points("h", "logrotate.timer", 100, 200);
        assert_eq!(pts.len(), 2);
        // No next elapse (u64::MAX) → only last_trigger.
        let pts = timer_points("h", "logrotate.timer", 100, u64::MAX);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].1.metric, "unit/logrotate.timer/last_trigger_usec");
    }

    #[test]
    fn mount_points_counts_by_state() {
        let pts = mount_points("h", ["active", "mounted", "failed", "inactive"]);
        let by: std::collections::HashMap<_, _> = pts
            .iter()
            .map(|(_, p)| (p.metric.as_str(), &p.value))
            .collect();
        assert_eq!(by["mounts/total"], &TelemetryValue::Gauge(4.0));
        assert_eq!(by["mounts/mounted"], &TelemetryValue::Gauge(2.0));
        assert_eq!(by["mounts/failed"], &TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn journal_points_gates_available() {
        assert_eq!(journal_points("h", 1024, Some(2048)).len(), 2);
        let one = journal_points("h", 1024, None);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].1.metric, "journal/disk_usage_bytes");
    }

    /// The rate derivation this module used to own moved to
    /// `zensight_sensor_core::rate::CounterTracker` (#1152). The properties it
    /// guarded are the same, and are asserted here against the shared type so
    /// this crate notices if they ever stop holding.
    #[test]
    fn counter_rates_still_guard_a_unit_restart() {
        use std::time::{Duration, Instant};
        use zensight_sensor_core::rate::CounterTracker;
        let t0 = Instant::now();
        let mut t: CounterTracker<&str> = CounterTracker::new();
        t.observe("u", 10_000, t0);
        // 10_000 bytes over 2 s = 5000 B/s.
        assert_eq!(
            t.observe("u", 20_000, t0 + Duration::from_secs(2))
                .map(|r| r.per_sec),
            Some(5000.0)
        );
        // A unit restart resets its IPAccounting counters: a backwards step is
        // a re-baseline, not a negative rate.
        assert!(
            t.observe("u", 500, t0 + Duration::from_secs(4)).is_none(),
            "a backwards step carries no rate"
        );
        // Non-advancing clock → no rate.
        assert!(
            t.observe("u", 900, t0 + Duration::from_secs(4)).is_none(),
            "and neither does a clock that did not move"
        );
    }

    #[test]
    fn ip_rate_points_are_labelled_wire_l3() {
        let pts = ip_rate_points("h", "nginx.service", Some(1000.0), Some(250.0), false);
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|(_, p)| (p.metric.as_str(), p)).collect();
        let ing = by["unit/nginx.service/ip_ingress_bps"];
        assert_eq!(ing.value, TelemetryValue::Gauge(1000.0));
        assert_eq!(
            ing.labels.get("bw.source").map(String::as_str),
            Some("systemd")
        );
        assert_eq!(
            ing.labels.get("bw.semantics").map(String::as_str),
            Some("wire-l3")
        );
        assert!(by.contains_key("unit/nginx.service/ip_egress_bps"));
        // No accounting-off marker when accounting is on.
        assert!(!by.contains_key("unit/nginx.service/ip_accounting"));
    }

    #[test]
    fn ip_rate_points_surface_accounting_off_not_zero() {
        // No bps (unknown), accounting off → emit the boolean marker, not a 0 bps.
        let pts = ip_rate_points("h", "sshd.service", None, None, true);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].1.metric, "unit/sshd.service/ip_accounting");
        assert_eq!(pts[0].1.value, TelemetryValue::Boolean(false));
    }
}
