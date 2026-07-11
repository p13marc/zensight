//! Pure mapping from ZenSight domain types to Rerun-free sink inputs.
//!
//! No `rerun` types anywhere in this module (grep-gated); the entity-path
//! scheme and classification rules are docs/plans/rerun/02-mapping.md made
//! executable, unit-tested without the Rerun dependency.

use std::collections::HashMap;

use zensight_common::alert::AlertSeverity;
use zensight_common::entity::HostEntity;
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

use crate::events::{EventKind, EventSeverity};

/// How a telemetry point should be visualized.
#[derive(Debug, Clone, PartialEq)]
pub enum Class {
    /// A numeric time series (`Scalars`); the payload is the plot value.
    Metric,
    /// A discrete occurrence (`TextLog` + attributes).
    Event(EventKind),
    /// Not visualizable (counted, never logged).
    Ignore,
}

/// Classify a telemetry point (02-mapping.md §2/§4).
///
/// 1. `Binary` → `Ignore` (no meaningful Rerun rendering).
/// 2. A metric path under `events/` is the sensors' discrete control-plane
///    timeline (e.g. netlink `events/ipsec/...`, `events/route/...`) → `Event`
///    with the kind derived from the path.
/// 3. `Text` → `Event(Other("text"))`, message = the text.
/// 4. `Gauge`/`Counter`/`Boolean` → `Metric`.
pub fn classify(point: &TelemetryPoint) -> Class {
    if matches!(point.value, TelemetryValue::Binary(_)) {
        return Class::Ignore;
    }
    if let Some(rest) = point.metric.strip_prefix("events/") {
        return Class::Event(event_kind_from_path(rest, &point.labels));
    }
    match &point.value {
        TelemetryValue::Text(_) => Class::Event(EventKind::Other("text".to_string())),
        _ => Class::Metric,
    }
}

/// Derive an [`EventKind`] from an `events/...` metric path (first chunk of
/// `rest`), falling back to an explicit `kind` label, then to `Other`.
fn event_kind_from_path(rest: &str, labels: &HashMap<String, String>) -> EventKind {
    let tag = labels
        .get("kind")
        .map(String::as_str)
        .unwrap_or_else(|| rest.split('/').next().unwrap_or(rest));
    match tag {
        "tcp_open" => EventKind::TcpOpen,
        "tcp_reset" | "reset" => EventKind::TcpReset,
        "tcp_timeout" | "timeout" => EventKind::TcpTimeout,
        "icmp_unreachable" | "unreachable" => EventKind::IcmpUnreachable,
        "packet_loss" => EventKind::PacketLoss,
        "link_degraded" => EventKind::LinkDegraded,
        "peer_up" => EventKind::PeerUp,
        "peer_down" => EventKind::PeerDown,
        "route_change" | "route" => EventKind::RouteChange,
        "restart" => EventKind::Restart,
        "anomaly" => EventKind::Anomaly,
        other => EventKind::Other(other.to_string()),
    }
}

/// Map an alert severity onto the event severity scale.
pub fn severity_from_alert(severity: AlertSeverity) -> EventSeverity {
    match severity {
        AlertSeverity::Info => EventSeverity::Info,
        AlertSeverity::Warning => EventSeverity::Warning,
        AlertSeverity::Critical => EventSeverity::Critical,
    }
}

/// The step-series weight of a firing alert (0.0 = resolved) — the
/// `alerts/<proto>/<source>/<alert_key>/state` lane (02-mapping.md §5).
pub fn severity_weight(severity: AlertSeverity) -> f64 {
    match severity {
        AlertSeverity::Info => 1.0,
        AlertSeverity::Warning => 2.0,
        AlertSeverity::Critical => 3.0,
    }
}

/// Live `(sensor, source) → entity_id` join, maintained from the
/// `zensight/_meta/entity/**` subscription. Alias ids resolve to the
/// surviving entity (02-mapping.md §1).
#[derive(Debug, Default)]
pub struct EntityIndex {
    /// `(member.sensor, member.source)` → entity id.
    members: HashMap<(String, String), String>,
    /// alias id → surviving entity id.
    aliases: HashMap<String, String>,
}

impl EntityIndex {
    /// Ingest (or refresh) one entity document.
    pub fn upsert(&mut self, entity: &HostEntity) {
        for alias in &entity.aliases {
            self.aliases.insert(alias.clone(), entity.entity_id.clone());
        }
        for member in &entity.members {
            self.members.insert(
                (member.sensor.clone(), member.source.clone()),
                entity.entity_id.clone(),
            );
        }
    }

    /// Resolve a telemetry `(protocol-as-sensor, source)` to its entity id,
    /// following one level of alias indirection.
    pub fn resolve(&self, sensor: &str, source: &str) -> Option<&str> {
        let id = self
            .members
            .get(&(sensor.to_string(), source.to_string()))?;
        Some(self.aliases.get(id).map(String::as_str).unwrap_or(id))
    }

    /// Number of member claims currently indexed.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether no member claim is indexed yet.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

/// Per-series counter→per-second-rate conversion (02-mapping.md §2).
///
/// Keyed by the concrete series (entity path) so two hosts' `rx_bytes` never
/// cross-contaminate. Returns `None` — absorb, don't plot — for:
/// - the first sample of a series (nothing to differentiate against),
/// - a reset (`value < previous`: restart or wrap) — re-arms on the new
///   baseline, one silently absorbed gap instead of a huge negative spike,
/// - an out-of-order / non-advancing clock (`timestamp <= previous`) —
///   absorbed WITHOUT touching the baseline, so a late sample can't shrink
///   the next delta's window and spike the rate.
#[derive(Debug, Default)]
pub struct RateConverter {
    last: HashMap<String, (i64, u64)>,
}

impl RateConverter {
    /// Feed one counter sample; get the per-second rate if computable.
    /// `timestamp_ms` is epoch milliseconds (rates come out per second).
    pub fn rate(&mut self, series: &str, timestamp_ms: i64, value: u64) -> Option<f64> {
        match self.last.get(series).copied() {
            None => {
                // First sample: becomes the baseline, nothing to plot yet.
                self.last.insert(series.to_string(), (timestamp_ms, value));
                None
            }
            Some((t0, _)) if timestamp_ms <= t0 => {
                // Out-of-order / non-advancing clock: absorb WITHOUT touching
                // the baseline — a late sample must not regress it, or the
                // next in-order delta computes over a shrunken window and
                // spikes. (Checked before reset detection: a stale sample
                // with a lower value is out-of-order, not a reset.)
                None
            }
            Some((_, v0)) if value < v0 => {
                // Reset (restart or wrap): re-arm on the new baseline — one
                // silently absorbed gap instead of a huge negative spike.
                self.last.insert(series.to_string(), (timestamp_ms, value));
                None
            }
            Some((t0, v0)) => {
                self.last.insert(series.to_string(), (timestamp_ms, value));
                Some((value - v0) as f64 * 1000.0 / (timestamp_ms - t0) as f64)
            }
        }
    }
}

/// Per-series sample-rate limiter (03-sink-design.md §4). Runs *before* the
/// [`RateConverter`], so sub-sampled counters still yield correct rates over
/// the longer window.
#[derive(Debug)]
pub struct Sampler {
    config: crate::config::SamplingConfig,
    last_emit: HashMap<String, i64>,
}

impl Sampler {
    pub fn new(config: crate::config::SamplingConfig) -> Self {
        Self {
            config,
            last_emit: HashMap::new(),
        }
    }

    /// The ceiling (Hz) for a metric path: longest matching `per_prefix`
    /// entry wins, else the global `max_hz_per_series`, else unlimited.
    fn limit_for(&self, metric: &str) -> Option<f64> {
        self.config
            .per_prefix
            .iter()
            .filter(|(prefix, _)| metric.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, hz)| *hz)
            .or(self.config.max_hz_per_series)
    }

    /// Whether a sample on `series` (limits chosen by `metric`) may pass now.
    pub fn allow(&mut self, series: &str, metric: &str, timestamp_ms: i64) -> bool {
        let Some(hz) = self.limit_for(metric) else {
            return true;
        };
        let min_interval_ms = (1000.0 / hz) as i64;
        match self.last_emit.get(series) {
            // Domain clock stepped backwards (NTP): accept and re-anchor the
            // high-water mark, otherwise the series stays suppressed until
            // time re-passes the old mark.
            Some(&last) if timestamp_ms < last => {
                self.last_emit.insert(series.to_string(), timestamp_ms);
                true
            }
            // Too-soon samples are suppressed.
            Some(&last) if timestamp_ms - last < min_interval_ms => false,
            _ => {
                self.last_emit.insert(series.to_string(), timestamp_ms);
                true
            }
        }
    }
}

/// Build the Rerun entity path for a metric series: correlated sources land
/// under `hosts/<entity_id>/…`, everything else under `sensors/…`
/// (02-mapping.md §1).
pub fn metric_entity_path(point: &TelemetryPoint, index: &EntityIndex) -> String {
    let protocol = point.protocol.as_str();
    match index.resolve(protocol, &point.source) {
        Some(entity_id) => format!("hosts/{entity_id}/{protocol}/{}", point.metric),
        None => format!("sensors/{protocol}/{}/{}", point.source, point.metric),
    }
}

/// Build the Rerun entity path for a source's discrete-event lane.
pub fn event_entity_path(protocol: &str, source: &str, index: &EntityIndex) -> String {
    event_entity_path_resolved(protocol, source, index.resolve(protocol, source))
}

/// [`event_entity_path`] with the entity-index lookup already done — lets a
/// caller that also needs the `entity_id` resolve once and derive both.
pub fn event_entity_path_resolved(protocol: &str, source: &str, entity_id: Option<&str>) -> String {
    match entity_id {
        Some(entity_id) => format!("hosts/{entity_id}/events"),
        None => format!("sensors/{protocol}/{source}/events"),
    }
}

/// Build the Rerun entity path for an alert's lifecycle lane:
/// `alerts/<protocol>/<source>/<alert_key>` (02-mapping.md §5).
///
/// The `alert_key` (FNV-1a over source+rule+identity labels, prefixed with the
/// sanitized rule name) is the alert's *identity*: two hosts firing the same
/// rule must not share a lane, or one host's Resolved (state weight 0.0)
/// would visually resolve the other's still-firing alert.
pub fn alert_entity_path(alert: &zensight_common::alert::Alert) -> String {
    format!(
        "alerts/{}/{}/{}",
        alert.protocol.as_str(),
        alert.source,
        alert.alert_key()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::entity::MemberClaim;
    use zensight_common::telemetry::Protocol;

    fn point(metric: &str, value: TelemetryValue) -> TelemetryPoint {
        TelemetryPoint::new("host1", Protocol::Sysinfo, metric, value)
    }

    #[test]
    fn classify_rules() {
        assert_eq!(
            classify(&point("cpu/usage", TelemetryValue::Gauge(1.0))),
            Class::Metric
        );
        assert_eq!(
            classify(&point("iface/eth0/rx_bytes", TelemetryValue::Counter(9))),
            Class::Metric
        );
        assert_eq!(
            classify(&point("power/on_battery", TelemetryValue::Boolean(true))),
            Class::Metric
        );
        assert_eq!(
            classify(&point("os/version", TelemetryValue::Text("x".into()))),
            Class::Event(EventKind::Other("text".to_string()))
        );
        assert_eq!(
            classify(&point("blob", TelemetryValue::Binary(vec![1]))),
            Class::Ignore
        );
        // events/ paths become events even with numeric payloads.
        assert_eq!(
            classify(&point("events/route/replace", TelemetryValue::Counter(1))),
            Class::Event(EventKind::RouteChange)
        );
        // an explicit kind label wins over the path chunk.
        let mut p = point("events/misc", TelemetryValue::Counter(1));
        p.labels.insert("kind".into(), "peer_down".into());
        assert_eq!(classify(&p), Class::Event(EventKind::PeerDown));
    }

    fn entity(id: &str, aliases: &[&str], members: &[(&str, &str)]) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: None,
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            members: members
                .iter()
                .map(|(sensor, source)| MemberClaim {
                    sensor: sensor.to_string(),
                    source: source.to_string(),
                    rule: "host_id".into(),
                    confidence: 1.0,
                    last_seen: 1,
                })
                .collect(),
            status: None,
            last_updated: 1,
        }
    }

    #[test]
    fn entity_paths_follow_correlation() {
        let mut index = EntityIndex::default();
        let p = point("cpu/usage", TelemetryValue::Gauge(1.0));

        // Uncorrelated → sensors/ fallback.
        assert_eq!(
            metric_entity_path(&p, &index),
            "sensors/sysinfo/host1/cpu/usage"
        );

        // Correlated → hosts/<entity_id>.
        index.upsert(&entity("h_0123456789ab", &[], &[("sysinfo", "host1")]));
        assert_eq!(
            metric_entity_path(&p, &index),
            "hosts/h_0123456789ab/sysinfo/cpu/usage"
        );
        assert_eq!(
            event_entity_path("sysinfo", "host1", &index),
            "hosts/h_0123456789ab/events"
        );
        assert_eq!(
            event_entity_path("netlink", "other", &index),
            "sensors/netlink/other/events"
        );
    }

    #[test]
    fn alias_resolves_to_surviving_entity() {
        let mut index = EntityIndex::default();
        // Weak entity first...
        index.upsert(&entity("h_aaaaaaaaaaaa", &[], &[("netlink", "hostA")]));
        assert_eq!(index.resolve("netlink", "hostA"), Some("h_aaaaaaaaaaaa"));
        // ...upgraded: surviving entity subsumes it (old id in aliases).
        index.upsert(&entity(
            "h_bbbbbbbbbbbb",
            &["h_aaaaaaaaaaaa"],
            &[("netlink", "hostA"), ("sysinfo", "hostA")],
        ));
        assert_eq!(index.resolve("netlink", "hostA"), Some("h_bbbbbbbbbbbb"));
        assert_eq!(index.resolve("sysinfo", "hostA"), Some("h_bbbbbbbbbbbb"));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn rate_converter_first_reset_and_clock() {
        let mut rc = RateConverter::default();
        // First sample: absorbed.
        assert_eq!(rc.rate("s", 1_000, 1_000), None);
        // 1000 bytes over 1s → 1000/s; ms→s scaling.
        assert_eq!(rc.rate("s", 2_000, 2_000), Some(1_000.0));
        // 500 over 500ms → 1000/s.
        assert_eq!(rc.rate("s", 2_500, 2_500), Some(1_000.0));
        // Reset (value drops): absorbed, re-armed on the new baseline...
        assert_eq!(rc.rate("s", 3_000, 100), None);
        // ...and the next delta differentiates against the new baseline.
        assert_eq!(rc.rate("s", 4_000, 1_100), Some(1_000.0));
        // Non-advancing clock: absorbed.
        assert_eq!(rc.rate("s", 4_000, 2_000), None);
        // Independent series don't cross-contaminate.
        assert_eq!(rc.rate("other", 10_000, 5), None);
        assert_eq!(rc.rate("other", 11_000, 10), Some(5.0));
    }

    #[test]
    fn rate_converter_out_of_order_keeps_baseline() {
        let mut rc = RateConverter::default();
        assert_eq!(rc.rate("s", 1_000, 1_000), None); // baseline (1000, 1000)
        assert_eq!(rc.rate("s", 2_000, 2_000), Some(1_000.0)); // baseline (2000, 2000)
        // A late (out-of-order) sample is absorbed WITHOUT regressing the
        // baseline — even when its lower value looks like a reset.
        assert_eq!(rc.rate("s", 1_500, 500), None);
        // The next in-order sample differentiates against (2000, 2000):
        // (2500 - 2000) / 1s = 500/s — not the 1333/s spike a regressed
        // (1500, 500) baseline would produce.
        assert_eq!(rc.rate("s", 3_000, 2_500), Some(500.0));
        // Same for a late sample with a *higher* value.
        assert_eq!(rc.rate("s", 2_500, 10_000), None);
        assert_eq!(rc.rate("s", 4_000, 3_500), Some(1_000.0));
    }

    #[test]
    fn sampler_recovers_from_backwards_clock_step() {
        use crate::config::SamplingConfig;

        // 2 Hz → min interval 500 ms.
        let mut s = Sampler::new(SamplingConfig {
            max_hz_per_series: Some(2.0),
            per_prefix: HashMap::new(),
        });
        assert!(s.allow("a", "cpu/usage", 10_000));
        // Domain clock steps backwards (NTP): the sample is accepted and the
        // high-water mark re-anchors — the series must not go dark until
        // time re-passes 10_000.
        assert!(s.allow("a", "cpu/usage", 1_000));
        // Rate limiting continues from the new anchor.
        assert!(!s.allow("a", "cpu/usage", 1_200));
        assert!(s.allow("a", "cpu/usage", 1_500));
    }

    #[test]
    fn alert_paths_include_identity() {
        use zensight_common::alert::{Alert, AlertKind, AlertSeverity};

        let alert = |source: &str| {
            Alert::new(
                source,
                Protocol::Netlink,
                AlertKind::Expectation,
                "ssh-listening",
                AlertSeverity::Critical,
                "sshd not listening",
            )
        };
        let a = alert("host-a");
        let b = alert("host-b");
        assert_eq!(
            alert_entity_path(&a),
            format!("alerts/netlink/host-a/{}", a.alert_key())
        );
        // Two hosts firing the same rule get distinct lanes...
        assert_ne!(alert_entity_path(&a), alert_entity_path(&b));
        // ...while firing and resolved transitions of one alert share theirs.
        assert_eq!(alert_entity_path(&a), alert_entity_path(&a.resolved()));
    }

    #[test]
    fn sampler_global_and_prefix_limits() {
        use crate::config::SamplingConfig;

        // Unlimited by default.
        let mut s = Sampler::new(SamplingConfig::default());
        assert!(s.allow("a", "cpu/usage", 0));
        assert!(s.allow("a", "cpu/usage", 1));

        // Global 2 Hz → min interval 500 ms, per series.
        let mut s = Sampler::new(SamplingConfig {
            max_hz_per_series: Some(2.0),
            per_prefix: HashMap::new(),
        });
        assert!(s.allow("a", "cpu/usage", 0));
        assert!(!s.allow("a", "cpu/usage", 100));
        assert!(s.allow("b", "cpu/usage", 100)); // other series unaffected
        assert!(s.allow("a", "cpu/usage", 500));

        // Longest-prefix override wins over the global limit.
        let mut per_prefix = HashMap::new();
        per_prefix.insert("iface".to_string(), 0.5); // one per 2s
        per_prefix.insert("iface/eth0".to_string(), 10.0); // one per 100ms
        let mut s = Sampler::new(SamplingConfig {
            max_hz_per_series: Some(2.0),
            per_prefix,
        });
        assert!(s.allow("x", "iface/eth1/rx_bytes", 0));
        assert!(!s.allow("x", "iface/eth1/rx_bytes", 1_000)); // 0.5 Hz
        assert!(s.allow("x", "iface/eth1/rx_bytes", 2_000));
        assert!(s.allow("y", "iface/eth0/rx_bytes", 0));
        assert!(s.allow("y", "iface/eth0/rx_bytes", 100)); // 10 Hz
    }

    #[test]
    fn severity_maps() {
        assert_eq!(severity_weight(AlertSeverity::Info), 1.0);
        assert_eq!(severity_weight(AlertSeverity::Warning), 2.0);
        assert_eq!(severity_weight(AlertSeverity::Critical), 3.0);
        assert_eq!(
            severity_from_alert(AlertSeverity::Critical),
            EventSeverity::Critical
        );
    }
}
