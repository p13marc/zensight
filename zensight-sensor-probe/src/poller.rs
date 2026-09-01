//! The probe loop (#820).
//!
//! Each target keeps its own schedule, because a certificate file worth
//! checking hourly and a URL worth checking every minute should not share a
//! cadence. A semaphore caps how many checks are in flight at once across all
//! of them, so a config with two hundred targets does not become a burst.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::probe::{ProbeOutcome, ProbeResult};
use zensight_common::{QosClass, TelemetryValue};
use zensight_sensor_core::{AdvancedPublisherRegistry, AlertReporter, Publisher, SensorHealth};

use crate::alerts;
use crate::config::{ProbeConfig, Target};
use crate::telemetry_guard::checked_point;

pub const STATE_QOS: QosClass = QosClass::HealthLiveness;

pub struct Poller {
    cfg: ProbeConfig,
    vantage: String,
    source: String,
    client: reqwest::Client,
    limit: Arc<tokio::sync::Semaphore>,
    publisher: Publisher,
    states: Arc<AdvancedPublisherRegistry>,
    reporter: Option<Arc<AlertReporter>>,
    health: Arc<SensorHealth>,
    /// When each target is next due.
    due: HashMap<String, Instant>,
    /// The last result per target, so a sweep that checks only some targets
    /// still grades the whole set — otherwise every rule would resolve and
    /// re-fire on the targets that were not due this tick.
    last: HashMap<String, ProbeResult>,
}

impl Poller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: ProbeConfig,
        publisher: Publisher,
        states: Arc<AdvancedPublisherRegistry>,
        reporter: Option<Arc<AlertReporter>>,
        health: Arc<SensorHealth>,
    ) -> anyhow::Result<Self> {
        let vantage = cfg.resolved_vantage();
        let source = cfg.resolved_source();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!("zensight-sensor-probe/", env!("CARGO_PKG_VERSION")))
            // Redirect policy is per-target; the client allows the maximum and
            // the checker decides whether where it ended up is acceptable.
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self {
            limit: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent.max(1))),
            cfg,
            vantage,
            source,
            client,
            publisher,
            states,
            reporter,
            health,
            due: HashMap::new(),
            last: HashMap::new(),
        })
    }

    pub async fn run(mut self) {
        // The tick is the interval floor, not any target's interval: each
        // target decides for itself whether it is due.
        let mut tick = tokio::time::interval(Duration::from_secs(crate::config::MIN_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let started = Instant::now();
            let checked = self.sweep().await;
            if !checked.is_empty() {
                self.publish(&checked).await;
                self.health
                    .record_poll_duration(started.elapsed().as_millis() as u64);
            }
        }
    }

    /// Run every target that is due, concurrently but capped.
    pub async fn sweep(&mut self) -> Vec<ProbeResult> {
        let now = Instant::now();
        let due: Vec<Target> = self
            .cfg
            .targets
            .iter()
            .filter(|t| t.enabled)
            .filter(|t| self.due.get(&t.name).is_none_or(|at| *at <= now))
            .cloned()
            .collect();
        if due.is_empty() {
            return Vec::new();
        }

        let mut tasks = Vec::new();
        for t in due {
            let limit = self.limit.clone();
            let client = self.client.clone();
            let vantage = self.vantage.clone();
            let timeout = Duration::from_secs(t.timeout(self.cfg.timeout_secs));
            tasks.push(tokio::spawn(async move {
                let _permit = limit.acquire().await;
                crate::check::run(&t, &vantage, timeout, &client).await
            }));
        }

        let mut out = Vec::new();
        for task in tasks {
            match task.await {
                Ok(r) => {
                    if r.outcome.is_ok() {
                        self.health.record_device_success(&r.name);
                    } else {
                        self.health.record_device_failure(
                            &r.name,
                            r.error.as_deref().unwrap_or("check failed"),
                        );
                    }
                    let interval = self
                        .cfg
                        .targets
                        .iter()
                        .find(|t| t.name == r.name)
                        .map_or(self.cfg.interval_secs, |t| {
                            t.interval(self.cfg.interval_secs)
                        });
                    self.due.insert(
                        r.name.clone(),
                        Instant::now() + Duration::from_secs(interval),
                    );
                    out.push(r);
                }
                Err(e) => tracing::warn!(error = %e, "probe: a check task panicked"),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        self.health
            .set_devices_total(self.cfg.targets.iter().filter(|t| t.enabled).count() as u64);
        out
    }

    pub async fn publish(&mut self, results: &[ProbeResult]) {
        let mut published = 0u64;
        for r in results {
            // The operator's name is a foreign value and is slugged before it
            // can reach a key — the #843 boundary.
            let slug = zenkey::Chunk::slug(&r.name).to_string();
            let mut labels = HashMap::new();
            labels.insert("target".to_string(), r.target.clone());
            labels.insert("kind".to_string(), r.kind.to_string());
            labels.insert("vantage".to_string(), r.vantage.clone());

            let mut points: Vec<(String, f64)> = vec![
                (
                    format!("{slug}/up"),
                    if r.outcome.is_ok() { 1.0 } else { 0.0 },
                ),
                // Published on a timeout too: the duration IS the diagnosis.
                (
                    format!("{slug}/timeout"),
                    if r.outcome == ProbeOutcome::Timeout {
                        1.0
                    } else {
                        0.0
                    },
                ),
            ];
            if let Some(d) = r.duration_ms {
                points.push((format!("{slug}/duration_ms"), d));
            }
            if let Some(h) = &r.http {
                if let Some(s) = h.status {
                    points.push((format!("{slug}/http_status"), s as f64));
                }
                if let Some(t) = h.ttfb_ms {
                    points.push((format!("{slug}/http_ttfb_ms"), t));
                }
            }
            if let Some(t) = &r.tls {
                if let Some(d) = t.days_to_expiry {
                    points.push((format!("{slug}/tls_days_to_expiry"), d as f64));
                }
                // Only when something actually validated a chain. A PEM on
                // disk has none, and a 0 would read as "invalid".
                if let Some(v) = t.chain_valid {
                    points.push((format!("{slug}/tls_chain_valid"), if v { 1.0 } else { 0.0 }));
                }
            }
            if let Some(d) = &r.dns {
                points.push((format!("{slug}/dns_answers"), d.answers.len() as f64));
            }

            for (metric, value) in points {
                // `source` is the vantage point, never the target (#883). A
                // probe result is by construction *an observation made from
                // somewhere*: filing it under the target discards the one
                // thing this sensor exists to record, and makes two hosts
                // probing the same URL collide on one identity.
                let p = checked_point(&self.source, &metric, TelemetryValue::Gauge(value))
                    .with_labels(labels.clone());
                if self.publisher.publish(&metric, &p).await.is_ok() {
                    published += 1;
                }
            }

            if let Some(key) = state_key(&["target", &slug])
                && let Err(e) = self.states.publish_serializable(&key, r).await
            {
                tracing::warn!(target = %r.name, error = %e, "probe: result publish failed");
            }
            self.last.insert(r.name.clone(), r.clone());
        }

        let enabled = self.cfg.targets.iter().filter(|t| t.enabled).count();
        let failing = self.last.values().filter(|r| !r.outcome.is_ok()).count();
        for (metric, value) in [
            ("targets/total", enabled as f64),
            ("targets/failing", failing as f64),
        ] {
            let p = checked_point(&self.source, metric, TelemetryValue::Gauge(value));
            if self.publisher.publish(metric, &p).await.is_ok() {
                published += 1;
            }
        }
        self.health.record_metrics_published(published);

        if let Some(reporter) = &self.reporter {
            // Grade the LAST KNOWN result for every target, not only the ones
            // checked this tick — otherwise a target with a slow interval
            // would have its alerts resolved and re-fired on every fast tick.
            let all: Vec<ProbeResult> = self.last.values().cloned().collect();
            let firing = alerts::grade(&self.cfg.alerts, &self.source, &all);
            let mut by_rule: HashMap<String, Vec<String>> = HashMap::new();
            for a in &firing {
                by_rule
                    .entry(a.rule.clone())
                    .or_default()
                    .push(a.alert_key());
            }
            for a in firing {
                if let Err(e) = reporter.observe(a, None).await {
                    tracing::warn!(error = %e, "probe: alert publish failed");
                }
            }
            for rule in alerts::ALL_RULES {
                let still = by_rule.remove(*rule).unwrap_or_default();
                if let Err(e) = reporter.reconcile(rule, &still).await {
                    tracing::warn!(rule = %rule, error = %e, "probe: reconcile failed");
                }
            }
        }
    }
}

fn state_key(chunks: &[&str]) -> Option<String> {
    match zensight_sensor_core::v1::for_producer("probe").state_key(chunks) {
        Ok(k) => Some(k.into()),
        Err(e) => {
            tracing::warn!(chunks = ?chunks, error = %e, "probe: not a legal state subject");
            None
        }
    }
}
