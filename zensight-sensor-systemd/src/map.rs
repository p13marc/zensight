//! Pure per-unit telemetry mapping (#273): [`UnitSample`] → [`TelemetryPoint`]s
//! under `systemd/unit/<unit>/*`, plus the `systemd/other/*` overflow bucket.
//! Kept free of I/O so it is unit-testable without a bus.

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::unit::UnitSample;

/// Sanitize a unit name for use as a key-expression chunk: unit names can carry
/// `@` (templated units) and other chars that are awkward/reserved in a keyexpr.
/// The raw name is always carried in a `unit` label, so the key only needs to be
/// stable and safe.
pub fn sanitize_unit(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | ' ' | '#' | '?' | '*' | '$' | '@' => out.push('_'),
            _ => out.push(c),
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unit".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build the per-unit telemetry points for one watched unit. Every point carries
/// a `unit` label with the raw name; resource points are emitted only when the
/// unit has the matching accounting enabled (`Some`), and `exit_code` only when
/// the unit is failed.
pub fn unit_points(source: &str, s: &UnitSample) -> Vec<TelemetryPoint> {
    let slug = sanitize_unit(&s.name);
    let base = format!("unit/{slug}");
    let point = |metric: String, value: TelemetryValue| {
        TelemetryPoint::new(source, Protocol::Systemd, metric, value)
            .with_label("unit", s.name.clone())
    };

    let mut pts = vec![
        point(
            format!("{base}/active"),
            TelemetryValue::Boolean(s.is_active()),
        ),
        // Active/sub state as text; load_state rides as a label for context.
        TelemetryPoint::new(
            source,
            Protocol::Systemd,
            format!("{base}/state"),
            TelemetryValue::Text(s.active_state.clone()),
        )
        .with_label("unit", s.name.clone())
        .with_label("load_state", s.load_state.clone())
        .with_label("sub_state", s.sub_state.clone()),
        point(
            format!("{base}/restarts_total"),
            TelemetryValue::Counter(s.n_restarts as u64),
        ),
        point(
            format!("{base}/active_since_usec"),
            TelemetryValue::Gauge(s.active_enter_usec as f64),
        ),
    ];

    if let Some(mem) = s.mem_bytes {
        pts.push(point(
            format!("{base}/mem_bytes"),
            TelemetryValue::Gauge(mem as f64),
        ));
    }
    if let Some(cpu) = s.cpu_usec {
        // CPU time is monotonic → Counter.
        pts.push(point(
            format!("{base}/cpu_usec"),
            TelemetryValue::Counter(cpu),
        ));
    }
    if let Some(tasks) = s.tasks {
        pts.push(point(
            format!("{base}/tasks"),
            TelemetryValue::Gauge(tasks as f64),
        ));
    }
    // Exit code is only meaningful for a failed unit.
    if s.is_failed() {
        pts.push(point(
            format!("{base}/exit_code"),
            TelemetryValue::Gauge(s.exec_main_status as f64),
        ));
    }
    // Opt-in IP/IO accounting (present only when the unit enabled it).
    for (metric, val) in [
        ("ip_ingress_bytes", s.ip_ingress_bytes),
        ("ip_egress_bytes", s.ip_egress_bytes),
        ("io_read_bytes", s.io_read_bytes),
        ("io_write_bytes", s.io_write_bytes),
    ] {
        if let Some(v) = val {
            pts.push(point(
                format!("{base}/{metric}"),
                TelemetryValue::Counter(v),
            ));
        }
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
) -> Vec<TelemetryPoint> {
    let base = format!("unit/{}", sanitize_unit(name));
    let point = |metric: String, value: TelemetryValue| {
        TelemetryPoint::new(source, Protocol::Systemd, metric, value).with_label("unit", name)
    };
    vec![
        // n_accepted is monotonic (lifetime connections accepted) → Counter.
        point(
            format!("{base}/n_accepted"),
            TelemetryValue::Counter(n_accepted as u64),
        ),
        point(
            format!("{base}/n_connections"),
            TelemetryValue::Gauge(n_connections as f64),
        ),
        point(
            format!("{base}/n_refused"),
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
) -> Vec<TelemetryPoint> {
    let base = format!("unit/{}", sanitize_unit(name));
    let point = |metric: String, value: TelemetryValue| {
        TelemetryPoint::new(source, Protocol::Systemd, metric, value).with_label("unit", name)
    };
    let mut pts = vec![point(
        format!("{base}/last_trigger_usec"),
        TelemetryValue::Gauge(last_trigger_usec as f64),
    )];
    if next_elapse_usec != 0 && next_elapse_usec != u64::MAX {
        pts.push(point(
            format!("{base}/next_trigger_usec"),
            TelemetryValue::Gauge(next_elapse_usec as f64),
        ));
    }
    pts
}

/// Mount/automount state aggregates (#279, `collect.mounts`): `mounts/{total,
/// mounted,failed}` from the enumerated units. `states` is the `active_state` of
/// each `.mount`/`.automount` unit.
pub fn mount_points<'a>(
    source: &str,
    states: impl IntoIterator<Item = &'a str>,
) -> Vec<TelemetryPoint> {
    let (mut total, mut mounted, mut failed) = (0u64, 0u64, 0u64);
    for s in states {
        total += 1;
        match s {
            "active" | "mounted" => mounted += 1,
            "failed" => failed += 1,
            _ => {}
        }
    }
    let gauge = |metric: &str, v: u64| {
        TelemetryPoint::new(
            source,
            Protocol::Systemd,
            metric,
            TelemetryValue::Gauge(v as f64),
        )
    };
    vec![
        gauge("mounts/total", total),
        gauge("mounts/mounted", mounted),
        gauge("mounts/failed", failed),
    ]
}

/// Journal store health (#279, `collect.journal`): `journal/{disk_usage_bytes,
/// disk_available_bytes}`.
pub fn journal_points(
    source: &str,
    usage_bytes: u64,
    available_bytes: Option<u64>,
) -> Vec<TelemetryPoint> {
    let gauge = |metric: &str, v: f64| {
        TelemetryPoint::new(source, Protocol::Systemd, metric, TelemetryValue::Gauge(v))
    };
    let mut pts = vec![gauge("journal/disk_usage_bytes", usage_bytes as f64)];
    if let Some(avail) = available_bytes {
        pts.push(gauge("journal/disk_available_bytes", avail as f64));
    }
    pts
}

/// The `systemd/other/*` overflow bucket (#273): a single gauge counting the
/// units that are NOT individually streamed (total minus watched), so their
/// existence isn't lost to the watchlist scoping.
pub fn other_points(source: &str, unwatched_total: u64) -> Vec<TelemetryPoint> {
    vec![TelemetryPoint::new(
        source,
        Protocol::Systemd,
        "other/units_total",
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

    #[test]
    fn sanitize_handles_template_and_reserved() {
        assert_eq!(sanitize_unit("sshd.service"), "sshd.service");
        assert_eq!(sanitize_unit("user@1000.service"), "user_1000.service");
        assert_eq!(sanitize_unit("a b/c"), "a_b_c");
    }

    #[test]
    fn active_unit_points_shape_and_labels() {
        let pts = unit_points("host01", &sample("nginx.service"));
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|p| (p.metric.as_str(), p)).collect();
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
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|p| (p.metric.as_str(), &p.value)).collect();
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
        assert_eq!(pts[0].metric, "other/units_total");
        assert_eq!(pts[0].value, TelemetryValue::Gauge(512.0));
    }

    #[test]
    fn socket_points_shape() {
        let pts = socket_points("h", "sshd.socket", 12, 3, 1);
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|p| (p.metric.as_str(), &p.value)).collect();
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
            pts[0].labels.get("unit").map(String::as_str),
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
        assert_eq!(pts[0].metric, "unit/logrotate.timer/last_trigger_usec");
    }

    #[test]
    fn mount_points_counts_by_state() {
        let pts = mount_points("h", ["active", "mounted", "failed", "inactive"]);
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|p| (p.metric.as_str(), &p.value)).collect();
        assert_eq!(by["mounts/total"], &TelemetryValue::Gauge(4.0));
        assert_eq!(by["mounts/mounted"], &TelemetryValue::Gauge(2.0));
        assert_eq!(by["mounts/failed"], &TelemetryValue::Gauge(1.0));
    }

    #[test]
    fn journal_points_gates_available() {
        assert_eq!(journal_points("h", 1024, Some(2048)).len(), 2);
        let one = journal_points("h", 1024, None);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].metric, "journal/disk_usage_bytes");
    }
}
