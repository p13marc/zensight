//! Composite per-host **health score** (#130).
//!
//! Folds the signals already on a dashboard [`DeviceState`] — sensor liveness,
//! the sysinfo saturation/health-state (#97, USE), the logs units-in-failure
//! gauge (RED-ish), and any netring anomaly count — into one 0–100 number
//! (higher = healthier) and a coarse band for at-a-glance fleet triage.
//!
//! The scoring is a pure function over extracted inputs so it is unit-tested
//! without constructing widgets. Per-protocol devices each get their own score
//! today; a true cross-protocol host rollup arrives with the Host aggregate
//! (#128). Firing-alert severity will join the inputs once incidents are wired
//! (#129).

use iced::Color;

use zensight_common::{DeviceStatus, TelemetryValue};

use crate::view::dashboard::DeviceState;

/// Coarse health band for tinting and grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthBand {
    Healthy,
    Degraded,
    Critical,
    Unknown,
}

impl HealthBand {
    pub fn label(self) -> &'static str {
        match self {
            HealthBand::Healthy => "healthy",
            HealthBand::Degraded => "degraded",
            HealthBand::Critical => "critical",
            HealthBand::Unknown => "unknown",
        }
    }

    /// Triage color: green / amber / red / gray (shared status palette, D2).
    pub fn color(self) -> Color {
        match self {
            HealthBand::Healthy => crate::view::theme::STATUS_ONLINE,
            HealthBand::Degraded => crate::view::theme::STATUS_DEGRADED,
            HealthBand::Critical => crate::view::theme::STATUS_OFFLINE,
            HealthBand::Unknown => crate::view::theme::STATUS_UNKNOWN,
        }
    }
}

/// A composite health verdict: a 0–100 score (higher = healthier) and its band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthScore {
    pub value: u8,
    pub band: HealthBand,
}

impl HealthScore {
    /// A short badge label, e.g. `"health 82"` or `"health —"` when unknown.
    pub fn label(self) -> String {
        match self.band {
            HealthBand::Unknown => "health —".to_string(),
            _ => format!("health {}", self.value),
        }
    }
}

/// Map a 0–100 score to a band (Unknown is decided separately by `compute`).
fn band_for(value: u8) -> HealthBand {
    if value >= 70 {
        HealthBand::Healthy
    } else if value >= 40 {
        HealthBand::Degraded
    } else {
        HealthBand::Critical
    }
}

/// Compute the composite score from extracted signals. Pure + unit-tested.
///
/// Starts at 100 and applies penalties: liveness (Offline is fatal, Degraded
/// heavy), sysinfo health-state (`crit`/`warn`, falling back to the numeric
/// saturation score when no categorical state is published), logs units in
/// failure, and netring active anomalies. A device with `Unknown` liveness and
/// no telemetry-derived signal scores `Unknown` rather than a misleading number.
pub fn compute(
    status: DeviceStatus,
    health_state: Option<&str>,
    saturation: Option<f64>,
    log_units_failing: Option<f64>,
    netring_anomalies: Option<f64>,
) -> HealthScore {
    let has_signal = health_state.is_some()
        || saturation.is_some()
        || log_units_failing.is_some()
        || netring_anomalies.is_some();

    if status == DeviceStatus::Unknown && !has_signal {
        return HealthScore {
            value: 0,
            band: HealthBand::Unknown,
        };
    }

    let mut penalty = 0.0_f64;
    match status {
        DeviceStatus::Offline => penalty += 100.0,
        DeviceStatus::Degraded => penalty += 40.0,
        DeviceStatus::Online | DeviceStatus::Unknown => {}
    }

    // sysinfo USE saturation: categorical health-state if present, else the
    // numeric saturation score (0..=100, higher = more saturated/worse).
    match health_state {
        Some("crit") => penalty += 50.0,
        Some("warn") => penalty += 25.0,
        Some("ok") => {}
        _ => {
            if let Some(s) = saturation {
                penalty += s.clamp(0.0, 100.0) * 0.5;
            }
        }
    }

    // logs RED-ish: any unit in failure is a real signal; scale, capped.
    if let Some(f) = log_units_failing
        && f > 0.0
    {
        penalty += (10.0 + f * 10.0).min(40.0);
    }
    // netring anomalies: active anomaly count, scaled and capped.
    if let Some(a) = netring_anomalies
        && a > 0.0
    {
        penalty += (15.0 + a * 5.0).min(40.0);
    }

    let value = (100.0 - penalty).clamp(0.0, 100.0) as u8;
    HealthScore {
        value,
        band: band_for(value),
    }
}

/// Numeric value of a metric on a dashboard device, if Counter/Gauge.
fn metric_num(d: &DeviceState, metric: &str) -> Option<f64> {
    match d.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Counter(v)) => Some(*v as f64),
        Some(TelemetryValue::Gauge(v)) => Some(*v),
        _ => None,
    }
}

/// Text value of a metric on a dashboard device, if Text.
fn metric_text(d: &DeviceState, metric: &str) -> Option<String> {
    match d.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Score a dashboard device from its live signals (#130).
pub fn score_device(d: &DeviceState) -> HealthScore {
    compute(
        d.effective_status(),
        metric_text(d, "system/health_state").as_deref(),
        metric_num(d, "system/saturation_score"),
        metric_num(d, "logs/units_in_failure"),
        // netring exposes active anomalies under a couple of names depending on
        // build; either is treated as the anomaly signal.
        metric_num(d, "security/anomalies_active")
            .or_else(|| metric_num(d, "flow/anomalies_active")),
    )
}

/// Worst-first rank of a band for host folding: Critical (0) is worst, Unknown
/// (3) is "no data" and least informative.
fn band_rank(b: HealthBand) -> u8 {
    match b {
        HealthBand::Critical => 0,
        HealthBand::Degraded => 1,
        HealthBand::Healthy => 2,
        HealthBand::Unknown => 3,
    }
}

/// Composite health for a physical host (#128): the worst of its per-protocol
/// facet scores — the band that should drive triage. Among equal bands the lower
/// numeric value wins; `Unknown` only survives when every facet is Unknown.
pub fn score_host<'a>(facets: impl IntoIterator<Item = &'a DeviceState>) -> HealthScore {
    let mut worst: Option<HealthScore> = None;
    for f in facets {
        let s = score_device(f);
        worst = Some(match worst {
            None => s,
            Some(w) => {
                let (wr, sr) = (band_rank(w.band), band_rank(s.band));
                if sr < wr || (sr == wr && s.value < w.value) {
                    s
                } else {
                    w
                }
            }
        });
    }
    worst.unwrap_or(HealthScore {
        value: 0,
        band: HealthBand::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_with_no_pressure_is_healthy() {
        let s = compute(
            DeviceStatus::Online,
            Some("ok"),
            Some(5.0),
            Some(0.0),
            Some(0.0),
        );
        assert_eq!(s.value, 100);
        assert_eq!(s.band, HealthBand::Healthy);
    }

    #[test]
    fn offline_is_critical_zero() {
        let s = compute(DeviceStatus::Offline, None, None, None, None);
        assert_eq!(s.value, 0);
        assert_eq!(s.band, HealthBand::Critical);
    }

    #[test]
    fn unknown_with_no_signal_is_unknown() {
        let s = compute(DeviceStatus::Unknown, None, None, None, None);
        assert_eq!(s.band, HealthBand::Unknown);
        assert_eq!(s.label(), "health —");
    }

    #[test]
    fn degraded_and_crit_saturation_stacks_into_critical() {
        // Degraded (-40) + crit health-state (-50) = 10 → Critical.
        let s = compute(DeviceStatus::Degraded, Some("crit"), None, None, None);
        assert_eq!(s.value, 10);
        assert_eq!(s.band, HealthBand::Critical);
    }

    #[test]
    fn warn_saturation_lands_in_degraded_band() {
        // Online, warn (-25) → 75 healthy; add a failing log unit to cross down.
        let s = compute(DeviceStatus::Online, Some("warn"), None, Some(2.0), None);
        // 100 - 25 - (10 + 2*10=30) = 45 → Degraded.
        assert_eq!(s.value, 45);
        assert_eq!(s.band, HealthBand::Degraded);
    }

    #[test]
    fn numeric_saturation_used_when_no_categorical_state() {
        // No health_state → fall back to saturation 80 → -40 → 60 Degraded.
        let s = compute(DeviceStatus::Online, None, Some(80.0), None, None);
        assert_eq!(s.value, 60);
        assert_eq!(s.band, HealthBand::Degraded);
    }

    /// #128: a host's composite score is the worst of its facets; an empty host
    /// or all-Unknown facets score Unknown.
    #[test]
    fn host_score_is_worst_facet() {
        use crate::message::DeviceId;
        use zensight_common::Protocol;

        let facet = |proto: Protocol, status: DeviceStatus| {
            let mut d = DeviceState::new(DeviceId::new(proto, "h"));
            d.update_from_liveness(status, 0, None);
            d
        };
        // A healthy sysinfo facet + an offline netlink facet → host is Critical.
        let healthy = facet(Protocol::Sysinfo, DeviceStatus::Online);
        let offline = facet(Protocol::Netlink, DeviceStatus::Offline);
        let host = score_host([&healthy, &offline]);
        assert_eq!(host.band, HealthBand::Critical);
        assert_eq!(host.value, 0);

        // No facets → Unknown.
        assert_eq!(score_host([]).band, HealthBand::Unknown);
    }
}
