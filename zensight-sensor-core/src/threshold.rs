//! The threshold evaluator (#930, epic #901).
//!
//! One state machine, installed on the publish path, evaluating a sensor's
//! [`ThresholdsConfig`] against every point it emits — instead of thirteen
//! copies of the same machine in thirteen `alerts.rs` files, and instead of a
//! rule engine in a GUI whose alerts reach nothing.
//!
//! # Where it runs, and why that is two places
//!
//! Epic #901 assumed `Publisher::publish_to_key` was "the single choke point
//! every telemetry point in all ten publishing sensors passes through". It is
//! not: `Publisher::publish` appears **zero** times in sysinfo, netlink,
//! netring, snmp and logs combined. See
//! [`zensight_common::point_observer`] for the three real paths. The evaluator
//! is therefore a [`PointObserver`] installed on whichever registries a sensor
//! actually publishes through, and one `Arc` serves both.
//!
//! # Sync in, async out
//!
//! `observe_point` runs on the publish path of every telemetry point — sysinfo
//! alone emits hundreds a tick — so it must not block and must not `.await`.
//! It does the whole state machine synchronously and, only on a **transition**,
//! sends one message to a task that does the publishing.
//!
//! The channel is bounded and **drops on full, loudly**. An alert transition
//! is not worth stalling a sensor's measurement loop for, and a silent drop is
//! how a monitoring tool stops monitoring; the dropped count rides the sensor's
//! own publish counters where an operator already looks.
//!
//! # The state machine
//!
//! Per (rule, matched alert key):
//!
//! ```text
//!   Clear ──violates──▶ Pending(for) ──held for `for`──▶ Firing
//!     ▲                      │                             │
//!     └──recovers────────────┘                    recovers │
//!                                                          ▼
//!                                    (AlertReporter holds the recovery
//!                                     window — #929 — so the evaluator
//!                                     simply stops listing the key)
//! ```
//!
//! `Pending` lives in the [`AlertReporter`], not here: `observe(alert, for)`
//! already implements exactly "continuously observed for N", including the
//! forget-on-clear that makes it *continuous* rather than *seen once ≥ N ago*.
//! Duplicating it here would be a second, subtly different debounce. What the
//! evaluator owns is which keys are firing **right now**, so it can reconcile.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zensight_common::point_observer::PointObserver;
use zensight_common::threshold::{
    ThresholdRule, ThresholdsConfig, default_summary, render_summary,
};
use zensight_common::{Alert, AlertKind, Protocol, TelemetryPoint, TelemetryValue};

use crate::alert::AlertReporter;

/// How many pending alert transitions may queue before they are dropped.
///
/// Generous enough that a normal burst never touches it, small enough that a
/// runaway rule cannot grow it without bound. Chosen for the same reason every
/// other bound in the tree is: an unbounded queue is a leak with a nice name.
const TRANSITION_QUEUE: usize = 1024;

/// The topic chunk this whole surface hangs off: `@desired
/// {host}/<producer>/thresholds`, `state/<producer>/applied/thresholds`,
/// `@rpc/<producer>/thresholds{,/set}`. One name, so the three cannot drift.
pub const THRESHOLDS_TOPIC: &str = "thresholds";

/// One rule's verdict about one matched series.
#[derive(Debug)]
enum Transition {
    /// Currently violated. Carries the `for` window so the reporter can debounce.
    Firing(Box<Alert>, Duration),
    /// Every key still firing for this rule, so the reporter can resolve the rest.
    Reconcile {
        rule: String,
        still_firing: Vec<String>,
    },
}

/// The evaluator's own state: which alert keys each rule currently has firing.
#[derive(Debug, Default)]
struct RuleState {
    /// Alert keys violated as of the last point seen for this rule.
    firing: HashSet<String>,
}

/// The rule set and what it currently has firing, under one lock.
///
/// One lock rather than two: the config is read on every point and written
/// rarely, but taking a second lock per point to learn there are no rules is
/// exactly the cost the `any_rules` fast path exists to avoid.
#[derive(Debug)]
struct Inner {
    config: ThresholdsConfig,
    rules: HashMap<String, RuleState>,
}

/// Evaluates a sensor's threshold rules against every point it publishes.
#[derive(Debug)]
pub struct ThresholdEvaluator {
    /// The point's own `source` is what an alert is filed under — for a proxy
    /// sensor that is the polled device, not the host — so the evaluator does
    /// not carry one of its own.
    protocol: Protocol,
    /// Whether any rule is installed **right now**.
    ///
    /// The hot path. A sensor whose rule set is empty — which is every sensor
    /// out of the box — pays one relaxed load per point rather than a lock and
    /// an empty iteration. It cannot simply be "was empty at startup", because
    /// `@desired` and `@rpc` can add rules to a running sensor (#931).
    any_rules: std::sync::atomic::AtomicBool,
    inner: Mutex<Inner>,
    tx: tokio::sync::mpsc::Sender<Transition>,
    /// Transitions dropped because the queue was full — surfaced, never silent.
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl ThresholdEvaluator {
    /// Build an evaluator and the task that publishes what it decides.
    ///
    /// Returns the evaluator (install it with
    /// [`zensight_common::PublisherRegistry::set_observer`] and/or
    /// `AdvancedPublisherRegistry::with_thresholds`) and a future the caller
    /// spawns — `SensorRunner::spawn` is the right owner, so it dies with the
    /// sensor.
    pub fn new(
        config: ThresholdsConfig,
        protocol: Protocol,
        reporter: Arc<AlertReporter>,
    ) -> (
        Arc<Self>,
        impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(TRANSITION_QUEUE);
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let evaluator = Arc::new(Self {
            protocol,
            any_rules: std::sync::atomic::AtomicBool::new(!config.rules.is_empty()),
            inner: Mutex::new(Inner {
                config,
                rules: HashMap::new(),
            }),
            tx,
            dropped: dropped.clone(),
        });
        (evaluator, run(rx, reporter, dropped))
    }

    /// Whether no rule is installed right now.
    pub fn is_empty(&self) -> bool {
        !self.any_rules.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The effective rule set, for the `@rpc <topic>` read (#931).
    pub fn config(&self) -> ThresholdsConfig {
        self.inner.lock().unwrap().config.clone()
    }

    /// Replace the rule set — the `@desired` reconciler and `@rpc
    /// <topic>/set` both land here (#931).
    ///
    /// **A rule that is gone has its alerts retired.** Dropping the state
    /// silently would leave whatever it had firing on the bus with nothing
    /// left to reconcile it away — an alert nobody can clear, from a rule
    /// nobody can see. The same problem `with_known_rules` solves across a
    /// restart, solved here across an edit.
    pub fn set_config(&self, config: ThresholdsConfig) {
        let retired: Vec<Transition> = {
            let mut inner = self.inner.lock().unwrap();
            let surviving: HashSet<&str> = config.rules.iter().map(|r| r.name.as_str()).collect();
            let gone: Vec<String> = inner
                .rules
                .keys()
                .filter(|name| !surviving.contains(name.as_str()))
                .cloned()
                .collect();
            let transitions = gone
                .iter()
                .map(|name| Transition::Reconcile {
                    rule: format!("threshold:{name}"),
                    still_firing: Vec::new(),
                })
                .collect();
            for name in gone {
                inner.rules.remove(&name);
            }
            self.any_rules.store(
                !config.rules.is_empty(),
                std::sync::atomic::Ordering::Relaxed,
            );
            inner.config = config;
            transitions
        };
        for t in retired {
            self.send(t);
        }
    }

    /// Transitions dropped because the publish queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The whole synchronous decision for one point.
    ///
    /// Split out from [`PointObserver::observe_point`] so it is testable
    /// without a bus: it returns what it would have sent.
    fn decide(&self, point: &TelemetryPoint) -> Vec<Transition> {
        let Some(value) = numeric(&point.value) else {
            // Text and binary carry no threshold. Not an error — most of a
            // device's tree is descriptive.
            return Vec::new();
        };
        let labels: BTreeMap<String, String> = point
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // What a rule matches against is the point's labels **plus its
        // `source`** — which is what `ThresholdsConfig`'s doc promises and
        // what makes a proxy work: snmp, gnmi and modbus publish one series
        // per polled device, so `labels: { source: "switch-01" }` scopes a
        // rule to one of them and leaving it out matches them all. Without
        // this the promise was in the documentation and nowhere else (#931).
        //
        // It is a SEPARATE map from the one the alert carries. `source` is
        // already `alert.source`; adding it to the labels as well would put it
        // in `alert_key()`'s hash for no gain, and every label that reaches an
        // alert is a label that can fork its identity.
        let mut match_labels = labels.clone();
        match_labels.insert("source".to_string(), point.source.clone());

        let mut out = Vec::new();
        let inner = &mut *self.inner.lock().unwrap();
        let (config, state) = (&inner.config, &mut inner.rules);
        for rule in &config.rules {
            if !rule.matches(&point.metric, &match_labels) {
                continue;
            }
            let alert = self.build_alert(rule, point, value, &labels);
            let key = alert.alert_key();
            let entry = state.entry(rule.name.clone()).or_default();

            if rule.fires(value) {
                entry.firing.insert(key);
                out.push(Transition::Firing(
                    Box::new(alert),
                    Duration::from_secs(config.for_secs(rule)),
                ));
            } else if rule.recovered(value) {
                // Only on a genuine recovery. Between `clear` and `value` the
                // key stays listed, which is what makes value hysteresis mean
                // anything: the alert holds through the band.
                entry.firing.remove(&key);
                out.push(Transition::Reconcile {
                    rule: rule.alert_rule(),
                    still_firing: entry.firing.iter().cloned().collect(),
                });
            }
        }
        out
    }

    fn build_alert(
        &self,
        rule: &ThresholdRule,
        point: &TelemetryPoint,
        value: f64,
        labels: &BTreeMap<String, String>,
    ) -> Alert {
        let summary = match &rule.summary {
            Some(template) => render_summary(
                template,
                &point.metric,
                value,
                &point.source,
                rule.op,
                rule.value,
                labels,
            ),
            None => default_summary(rule, &point.metric, value, &point.source),
        };
        let mut alert = Alert::new(
            &point.source,
            self.protocol,
            AlertKind::Expectation,
            rule.alert_rule(),
            rule.severity,
            summary,
        )
        .with_label("metric", point.metric.clone());

        // The point's own labels ride along so a per-interface rule produces a
        // per-interface alert. The MEASURED VALUE deliberately does not: a
        // label that changes every sweep mints a new alert key every sweep,
        // which restarts the `for` clock so the rule can never fire, and
        // leaves a Put/Delete pair on the bus per sample. netlink's
        // `MetricExpectation` does exactly that today (#932 retires it).
        for (k, v) in labels {
            alert = alert.with_label(k.clone(), v.clone());
        }
        if let Some(unit) = &point.unit {
            alert = alert.with_label("unit", unit.clone());
        }
        alert
    }

    fn send(&self, transition: Transition) {
        if self.tx.try_send(transition).is_err() {
            // Loudly. A silent drop here is a monitoring tool that stopped
            // monitoring and did not say so.
            let n = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if n.is_power_of_two() {
                tracing::warn!(
                    dropped_total = n,
                    "threshold: the alert-transition queue is full; a transition was dropped. \
                     A rule matching a very high-cardinality metric is the usual cause"
                );
            }
        }
    }
}

/// Install a sensor's threshold rules (#930, #931).
///
/// The one call a sensor makes. It builds the evaluator, installs it on the
/// registries the sensor publishes through, and spawns the task that turns
/// transitions into alerts.
///
/// `extra` is for the registries a sensor owns itself —
/// `AdvancedPublisherRegistry` instances built in its `main.rs` — because the
/// runner cannot know about those. A sensor that publishes only through
/// `runner.publisher()` passes an empty slice.
///
/// The observer is installed **even with no rules in the file config**,
/// because `@desired` and `@rpc` can add them to a running sensor (#931).
/// That costs one relaxed atomic load per point while the set is empty — the
/// `any_rules` fast path — rather than the lock and the empty iteration a
/// naive check would pay.
pub fn install<C: crate::config::SensorConfig>(
    runner: &mut crate::runner::SensorRunner<C>,
    config: ThresholdsConfig,
    protocol: Protocol,
    reporter: Arc<AlertReporter>,
    extra: &[Arc<crate::AdvancedPublisherRegistry>],
) -> Arc<ThresholdEvaluator> {
    let count = config.rules.len();
    let (evaluator, task) = ThresholdEvaluator::new(config, protocol, reporter);

    runner.publisher().set_observer(evaluator.clone());
    for registry in extra {
        registry.set_observer(evaluator.clone());
    }
    runner.spawn(task);
    tracing::info!(rules = count, "threshold evaluator installed");
    evaluator
}

/// The whole of a sensor's threshold adoption, in one call (#931).
///
/// [`install`] puts the evaluator on the publish path. This puts the *rule
/// set* on the bus: the `@desired` reconciler that lets a controller author
/// it fleet-wide, the `@rpc/<producer>/thresholds` read and
/// `…/thresholds/set` write that let an operator author it for one host, and
/// the `applied/thresholds` marker that says which of the three writers —
/// file, desired, rpc — is actually in force.
///
/// It runs **whether or not the file config carries a `thresholds` block**.
/// That is the #849 lesson, learned the expensive way on systemd: gating the
/// reconciler on the file block meant a stock install (whose shipped config
/// comments the block out) never subscribed, never seeded and never published
/// the marker at all — and the primary case for `@desired` is precisely a
/// host with *no* local set that is supposed to receive one.
///
/// `desired_key` comes from the caller's generated registry
/// (`registry::desired::key(&Subject::<producer>_thresholds(host_id))`), which
/// is what keeps this crate registry-agnostic.
pub async fn adopt<C: crate::config::SensorConfig>(
    runner: &mut crate::runner::SensorRunner<C>,
    protocol: Protocol,
    reporter: Arc<AlertReporter>,
    desired_key: zenkey::Key,
    extra: &[Arc<crate::AdvancedPublisherRegistry>],
) -> crate::Result<Arc<ThresholdEvaluator>> {
    let config = runner.config().thresholds();
    // The same gate both writers run. A file config that is invalid is a
    // startup error — the operator is at the keyboard — where an invalid
    // desired document is refused and rides the marker instead.
    config
        .validate()
        .map_err(|e| crate::SensorError::validation(format!("thresholds is invalid: {e}")))?;

    let baseline = config.clone();
    let evaluator = install(runner, config, protocol, reporter, extra);

    // The @desired reconciler and the @rpc surface are two writers to one
    // evaluator; the shared marker is what says who won last.
    let desired_cfg = runner.config().desired();
    let apply_to = evaluator.clone();
    let (marker, _task) = crate::desired::reconcile_topic(
        runner.session().clone(),
        runner.publisher(),
        crate::desired::DesiredTopic {
            topic: THRESHOLDS_TOPIC,
            desired_key,
        },
        desired_cfg,
        baseline,
        move |cfg: ThresholdsConfig| {
            let e = apply_to.clone();
            async move {
                // The SAME gate the RPC path runs: an invalid desired doc is
                // refused (kept off the evaluator) and rides the marker's
                // `last_rejected`, with the previous good set still running.
                cfg.validate()?;
                e.set_config(cfg);
                Ok(())
            }
        },
    );

    let producer = runner.publisher().v1().producer().name().to_string();
    let apply_marker = marker.clone();
    let apply_to = evaluator.clone();
    let read_from = evaluator.clone();
    let tasks = crate::rpc::serve_topic::<ThresholdsConfig, _, _, _, _>(
        runner.session().clone(),
        runner.publisher().v1(),
        THRESHOLDS_TOPIC,
        move |cfg: ThresholdsConfig| {
            let e = apply_to.clone();
            let m = apply_marker.clone();
            async move {
                cfg.validate().map_err(crate::rpc::RpcError::invalid_args)?;
                e.set_config(cfg.clone());
                m.publish(
                    zensight_common::desired::AppliedSource::Rpc,
                    &cfg,
                    None,
                    None,
                )
                .await;
                tracing::info!(
                    rules = cfg.rules.len(),
                    "threshold rule set replaced over @rpc"
                );
                Ok(())
            }
        },
        move || {
            let e = read_from.clone();
            let p = producer.clone();
            async move {
                serde_json::to_vec(&e.config())
                    .map_err(|err| crate::rpc::RpcError::producer(&p, "serialize", err.to_string()))
            }
        },
    )
    .await?;
    for t in tasks {
        runner.spawn(async move {
            let _ = t.await;
        });
    }

    Ok(evaluator)
}

impl PointObserver for ThresholdEvaluator {
    fn observe_point(&self, _key: &str, point: &TelemetryPoint) {
        // The fast path, and the reason this is an atomic rather than a lock:
        // a sensor with no rules — every sensor out of the box — pays one
        // relaxed load per point.
        if !self.any_rules.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        for transition in self.decide(point) {
            self.send(transition);
        }
    }
}

/// Drain transitions and publish them. One task per sensor.
async fn run(
    mut rx: tokio::sync::mpsc::Receiver<Transition>,
    reporter: Arc<AlertReporter>,
    _dropped: Arc<std::sync::atomic::AtomicU64>,
) {
    while let Some(transition) = rx.recv().await {
        match transition {
            Transition::Firing(alert, for_duration) => {
                if let Err(e) = reporter.observe(*alert, Some(for_duration)).await {
                    tracing::warn!(error = %e, "threshold: publishing an alert failed");
                }
            }
            Transition::Reconcile { rule, still_firing } => {
                if let Err(e) = reporter.reconcile(&rule, &still_firing).await {
                    tracing::warn!(error = %e, "threshold: reconcile failed");
                }
            }
        }
    }
}

/// A telemetry value as a number, or `None` for the ones that are not.
fn numeric(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Gauge(v) => Some(*v),
        TelemetryValue::Counter(v) => Some(*v as f64),
        // A boolean IS a number for threshold purposes — `up == 0` is the most
        // natural rule anyone will write, and refusing it would send them to
        // write `up < 1` instead.
        TelemetryValue::Boolean(b) => Some(f64::from(u8::from(*b))),
        TelemetryValue::Text(_) | TelemetryValue::Binary(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::comparison::ComparisonOp;

    fn point(metric: &str, value: f64) -> TelemetryPoint {
        TelemetryPoint::new(
            "web01",
            Protocol::Sysinfo,
            metric,
            TelemetryValue::Gauge(value),
        )
    }

    /// `decide` is pure with respect to the channel — only `observe_point`
    /// sends — so the receiver is simply dropped here.
    fn evaluator(rules: Vec<ThresholdRule>) -> ThresholdEvaluator {
        with_config(ThresholdsConfig {
            rules,
            ..Default::default()
        })
    }

    /// `source` is matchable as a label, and does NOT become one on the alert.
    ///
    /// The first half is what makes a proxy work: snmp, gnmi and modbus
    /// publish one series per polled device, so `labels: { source: … }` scopes
    /// a rule to one of them. It was documented from the start (#928) and did
    /// not work until #931 — `decide` matched against the point's labels
    /// alone, and `source` is not one of them.
    ///
    /// The second half is why it is a separate map: `source` is already
    /// `alert.source`, and every label that reaches an alert is a label
    /// `alert_key()` hashes. Two rules that differ only in their `source`
    /// pattern must not fork one device's alert identity.
    #[test]
    fn a_source_label_scopes_a_rule_without_entering_the_alert_key() {
        let mut scoped = ThresholdRule::new("hot", "cpu/usage", ComparisonOp::GreaterThan, 90.0);
        scoped.labels.insert("source".into(), "web01".into());
        let mut other = ThresholdRule::new("hot", "cpu/usage", ComparisonOp::GreaterThan, 90.0);
        other.labels.insert("source".into(), "db*".into());

        let p = point("cpu/usage", 95.0);
        assert_eq!(
            evaluator(vec![scoped]).decide(&p).len(),
            1,
            "a rule naming this source must fire on it"
        );
        assert!(
            evaluator(vec![other]).decide(&p).is_empty(),
            "a rule naming a different source must not"
        );

        // …and the alert it produces carries no `source` label, so the key is
        // the same one an unscoped rule of the same name would mint.
        let mut scoped = ThresholdRule::new("hot", "cpu/usage", ComparisonOp::GreaterThan, 90.0);
        scoped.labels.insert("source".into(), "web01".into());
        let unscoped = ThresholdRule::new("hot", "cpu/usage", ComparisonOp::GreaterThan, 90.0);
        let key_of = |rule: ThresholdRule| match evaluator(vec![rule]).decide(&p).remove(0) {
            Transition::Firing(alert, _) => {
                assert!(
                    !alert.labels.contains_key("source"),
                    "source must not ride the alert's labels: {:?}",
                    alert.labels
                );
                alert.alert_key()
            }
            other => panic!("expected a firing transition, got {other:?}"),
        };
        assert_eq!(key_of(scoped), key_of(unscoped));
    }

    fn with_config(config: ThresholdsConfig) -> ThresholdEvaluator {
        let (tx, _rx) = tokio::sync::mpsc::channel(TRANSITION_QUEUE);
        ThresholdEvaluator {
            protocol: Protocol::Sysinfo,
            any_rules: std::sync::atomic::AtomicBool::new(!config.rules.is_empty()),
            inner: Mutex::new(Inner {
                config,
                rules: HashMap::new(),
            }),
            tx,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn rule() -> ThresholdRule {
        ThresholdRule::new(
            "cpu-hot",
            "cpu/usage_percent",
            ComparisonOp::GreaterThan,
            90.0,
        )
    }

    fn firing_alert(ts: &[Transition]) -> Option<&Alert> {
        ts.iter().find_map(|t| match t {
            Transition::Firing(a, _) => Some(a.as_ref()),
            _ => None,
        })
    }

    impl Transition {
        fn is_reconcile(&self) -> bool {
            matches!(self, Transition::Reconcile { .. })
        }
    }

    fn reconcile_of<'a>(ts: &'a [Transition], rule: &str) -> Option<&'a Vec<String>> {
        ts.iter().find_map(|t| match t {
            Transition::Reconcile {
                rule: r,
                still_firing,
            } if r == rule => Some(still_firing),
            _ => None,
        })
    }

    /// A point that violates fires; one that does not, recovers. The whole
    /// job, in one test.
    #[test]
    fn a_violating_point_fires_and_a_healthy_one_recovers() {
        let e = evaluator(vec![rule()]);

        let ts = e.decide(&point("cpu/usage_percent", 95.0));
        let alert = firing_alert(&ts).expect("a violation fires");
        assert_eq!(alert.rule, "threshold:cpu-hot");
        assert_eq!(alert.source, "web01");
        assert_eq!(alert.labels["metric"], "cpu/usage_percent");
        assert!(alert.summary.contains("95"), "{}", alert.summary);

        let ts = e.decide(&point("cpu/usage_percent", 10.0));
        assert!(firing_alert(&ts).is_none());
        assert!(
            reconcile_of(&ts, "threshold:cpu-hot")
                .expect("a recovery reconciles")
                .is_empty(),
            "nothing is still firing"
        );
    }

    /// A metric no rule matches costs nothing and produces nothing — which is
    /// every metric on a sensor with a rule for one series.
    #[test]
    fn an_unmatched_metric_produces_nothing() {
        let e = evaluator(vec![rule()]);
        assert!(e.decide(&point("memory/used_bytes", 999.0)).is_empty());
    }

    /// No rules installed: the evaluator is inert, and `is_empty` lets a
    /// sensor skip installing it at all.
    #[test]
    fn with_no_rules_nothing_is_evaluated() {
        let e = evaluator(Vec::new());
        assert!(e.is_empty());
        assert!(e.decide(&point("cpu/usage_percent", 100.0)).is_empty());
    }

    /// **Value hysteresis, end to end.** In the band between `clear` and
    /// `value` the key stays listed as firing — neither a new fire nor a
    /// recovery — which is what makes the band mean anything.
    #[test]
    fn a_value_in_the_hysteresis_band_neither_fires_again_nor_recovers() {
        let mut r = rule();
        r.clear = Some(80.0);
        let e = evaluator(vec![r]);

        let ts = e.decide(&point("cpu/usage_percent", 95.0));
        let key = firing_alert(&ts).unwrap().alert_key();

        // 85: below 90, above 80. Nothing at all should be emitted.
        let ts = e.decide(&point("cpu/usage_percent", 85.0));
        assert!(ts.is_empty(), "the band emits nothing");
        assert!(
            e.inner.lock().unwrap().rules["cpu-hot"]
                .firing
                .contains(&key),
            "and the alert is still listed as firing"
        );

        // 75: through `clear`. Now it recovers.
        let ts = e.decide(&point("cpu/usage_percent", 75.0));
        assert!(reconcile_of(&ts, "threshold:cpu-hot").unwrap().is_empty());
    }

    /// **Two series under one rule do not resolve each other.** The
    /// proxy-sensor case: one rule over `if/*/in_errors.rate` covers every
    /// interface, and interface 3 recovering must not retract interface 4's
    /// alert.
    #[test]
    fn two_series_under_one_rule_reconcile_independently() {
        let mut r = rule();
        r.metric = "if/*/in_errors.rate".to_string();
        r.value = 1.0;
        let e = evaluator(vec![r]);

        let mut p3 = point("if/3/in_errors.rate", 5.0);
        p3.labels.insert("if_index".to_string(), "3".to_string());
        let mut p4 = point("if/4/in_errors.rate", 7.0);
        p4.labels.insert("if_index".to_string(), "4".to_string());

        let k3 = firing_alert(&e.decide(&p3)).unwrap().alert_key();
        let k4 = firing_alert(&e.decide(&p4)).unwrap().alert_key();
        assert_ne!(k3, k4, "different interfaces are different alerts");

        // Interface 3 recovers. The reconcile must still list interface 4.
        p3.value = TelemetryValue::Gauge(0.0);
        let ts = e.decide(&p3);
        let still = reconcile_of(&ts, "threshold:cpu-hot").expect("reconciles");
        assert_eq!(still, &vec![k4], "interface 4 is still firing");
    }

    /// A label glob scopes a rule to devices — the proxy sensor writing one
    /// rule for one PDU rather than for every device it polls.
    ///
    /// The device comes from `TelemetryPoint::source`, which is where a proxy
    /// actually puts it (#883: the payload `source` is the polled device).
    /// This test used to put `"source"` in `point.labels` instead — a shape no
    /// sensor emits — and so passed while the feature did not work at all
    /// (#931 found it, from snmp's side, with a real agent).
    #[test]
    fn a_label_glob_scopes_the_rule() {
        let mut r = rule();
        r.metric = "*/temperature".to_string();
        r.labels.insert("source".to_string(), "pdu-*".to_string());
        r.value = 40.0;
        let e = evaluator(vec![r]);

        let mut matching = point("rack/temperature", 50.0);
        matching.source = "pdu-a".to_string();
        assert!(firing_alert(&e.decide(&matching)).is_some());

        let mut other = point("rack/temperature", 50.0);
        other.source = "switch01".to_string();
        assert!(
            e.decide(&other).is_empty(),
            "a different device is not covered"
        );
    }

    /// **The measured value is not a label.** A label that changes every sweep
    /// mints a new alert key every sweep, which restarts the `for` clock so the
    /// rule can never fire and leaves a Put/Delete pair on the bus per sample.
    /// netlink's `MetricExpectation` does exactly that today.
    #[test]
    fn the_measured_value_never_becomes_a_label() {
        let e = evaluator(vec![rule()]);
        let first = firing_alert(&e.decide(&point("cpu/usage_percent", 95.0)))
            .unwrap()
            .clone();
        let second = firing_alert(&e.decide(&point("cpu/usage_percent", 96.0)))
            .unwrap()
            .clone();

        assert_eq!(
            first.alert_key(),
            second.alert_key(),
            "a changing value must not mint a new alert identity"
        );
        assert!(
            !first.labels.values().any(|v| v == "95"),
            "the value is in the summary, not the labels: {:?}",
            first.labels
        );
        assert_ne!(first.summary, second.summary, "but the summary does update");
    }

    /// Text and binary carry no threshold; a boolean does, because `up == 0`
    /// is the most natural rule anyone will write.
    #[test]
    fn only_numeric_values_are_evaluated_and_a_boolean_is_numeric() {
        assert_eq!(numeric(&TelemetryValue::Gauge(1.5)), Some(1.5));
        assert_eq!(numeric(&TelemetryValue::Counter(7)), Some(7.0));
        assert_eq!(numeric(&TelemetryValue::Boolean(true)), Some(1.0));
        assert_eq!(numeric(&TelemetryValue::Boolean(false)), Some(0.0));
        assert_eq!(numeric(&TelemetryValue::Text("up".into())), None);

        let e = evaluator(vec![ThresholdRule::new(
            "down",
            "link/up",
            ComparisonOp::LessThan,
            1.0,
        )]);
        let p = TelemetryPoint::new(
            "web01",
            Protocol::Sysinfo,
            "link/up",
            TelemetryValue::Boolean(false),
        );
        assert!(firing_alert(&e.decide(&p)).is_some());
    }

    /// A summary template renders against the point; without one, a generated
    /// sentence names the metric, the value and the test.
    #[test]
    fn the_summary_comes_from_the_rule_or_a_sensible_default() {
        let mut r = rule();
        r.summary = Some("{source} cpu at {value}% (limit {threshold})".to_string());
        let e = evaluator(vec![r]);
        let alert = firing_alert(&e.decide(&point("cpu/usage_percent", 97.0)))
            .unwrap()
            .clone();
        assert_eq!(alert.summary, "web01 cpu at 97% (limit 90)");

        let e = evaluator(vec![rule()]);
        let alert = firing_alert(&e.decide(&point("cpu/usage_percent", 97.0)))
            .unwrap()
            .clone();
        assert_eq!(alert.summary, "web01: cpu/usage_percent is 97 (> 90)");
    }

    /// The `for` window travels with the transition, so the reporter — which
    /// already implements "continuously observed for N", including the
    /// forget-on-clear that makes it *continuous* — does the debouncing. No
    /// second, subtly different implementation here.
    #[test]
    fn the_for_window_is_carried_to_the_reporter_not_reimplemented() {
        let mut r = rule();
        r.for_secs = Some(120);
        let e = evaluator(vec![r]);
        let ts = e.decide(&point("cpu/usage_percent", 95.0));
        match &ts[0] {
            Transition::Firing(_, d) => assert_eq!(*d, Duration::from_secs(120)),
            other => panic!("expected a firing transition, got {other:?}"),
        }
    }

    /// The set-wide default applies when a rule does not override it.
    #[test]
    fn the_set_wide_for_default_applies() {
        let e = with_config(ThresholdsConfig {
            default_for_secs: 300,
            rules: vec![rule()],
            ..Default::default()
        });
        match &e.decide(&point("cpu/usage_percent", 95.0))[0] {
            Transition::Firing(_, d) => assert_eq!(*d, Duration::from_secs(300)),
            other => panic!("expected a firing transition, got {other:?}"),
        }
    }

    /// **Removing a rule retires its alerts.** Dropping the state silently
    /// would leave whatever it had firing on the bus with nothing left to
    /// reconcile it away — an alert nobody can clear, from a rule nobody can
    /// see.
    #[test]
    fn a_rule_removed_by_a_config_swap_has_its_alerts_retired() {
        let e = evaluator(vec![rule()]);
        assert!(firing_alert(&e.decide(&point("cpu/usage_percent", 95.0))).is_some());
        assert!(e.inner.lock().unwrap().rules.contains_key("cpu-hot"));

        // Swap in a set that no longer has it. The removal must be announced.
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let e = ThresholdEvaluator {
            protocol: Protocol::Sysinfo,
            any_rules: std::sync::atomic::AtomicBool::new(true),
            inner: Mutex::new(Inner {
                config: ThresholdsConfig {
                    rules: vec![rule()],
                    ..Default::default()
                },
                rules: HashMap::new(),
            }),
            tx,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        e.decide(&point("cpu/usage_percent", 95.0));
        e.set_config(ThresholdsConfig::default());

        let sent = rx.try_recv().expect("the removal is announced");
        match sent {
            Transition::Reconcile { rule, still_firing } => {
                assert_eq!(rule, "threshold:cpu-hot");
                assert!(still_firing.is_empty(), "nothing survives a deleted rule");
            }
            other => panic!("expected a reconcile, got {other:?}"),
        }
        assert!(e.inner.lock().unwrap().rules.is_empty());
        assert!(e.is_empty(), "and the fast path goes back to free");
    }

    /// A rule that survives a swap keeps its state — its alert is not
    /// retracted and re-raised just because the document was rewritten.
    #[test]
    fn a_surviving_rule_keeps_its_firing_state_across_a_swap() {
        let e = evaluator(vec![rule()]);
        let key = firing_alert(&e.decide(&point("cpu/usage_percent", 95.0)))
            .unwrap()
            .alert_key();

        let mut edited = rule();
        edited.value = 80.0; // same name, different threshold
        e.set_config(ThresholdsConfig {
            rules: vec![edited],
            ..Default::default()
        });

        assert!(
            e.inner.lock().unwrap().rules["cpu-hot"]
                .firing
                .contains(&key),
            "the rule is still there, so its alert is still its own to resolve"
        );
    }

    /// `@desired` can add rules to a sensor that started with none, so the
    /// observer stays installed and the fast path flips.
    #[test]
    fn a_sensor_that_started_with_no_rules_can_be_given_some() {
        let e = evaluator(Vec::new());
        assert!(e.is_empty());
        assert!(e.decide(&point("cpu/usage_percent", 100.0)).is_empty());

        e.set_config(ThresholdsConfig {
            rules: vec![rule()],
            ..Default::default()
        });
        assert!(!e.is_empty());
        assert!(firing_alert(&e.decide(&point("cpu/usage_percent", 95.0))).is_some());
    }

    /// The effective set is readable, which is what `@rpc <topic>` serves.
    #[test]
    fn the_effective_set_reads_back() {
        let e = evaluator(vec![rule()]);
        assert_eq!(e.config().rules.len(), 1);
        assert_eq!(e.config().rules[0].name, "cpu-hot");
    }

    // ── the three tests that moved here from the GUI engine (#930) ────────
    //
    // They tested `AlertRule::matches`, `AlertRule::evaluate` and
    // `AlertsState::check_metric` — the local rule engine #934 deletes. The
    // behaviour they pinned belongs to a sensor now, so they are rewritten
    // against the evaluator rather than dropped.

    /// Was `test_alert_rule_matches`. Note the difference the move makes: the
    /// GUI matched with `metric.contains(pattern)`, so a rule for `in_errors`
    /// also matched `total_in_errors_dropped`. A glob says what it means.
    #[test]
    fn a_rule_matches_by_glob_not_by_substring() {
        let mut r = rule();
        r.metric = "if/*/in_errors".to_string();
        let e = evaluator(vec![r]);
        assert!(firing_alert(&e.decide(&point("if/1/in_errors", 100.0))).is_some());
        assert!(e.decide(&point("if/1/out_errors", 100.0)).is_empty());
        assert!(
            e.decide(&point("if/1/total_in_errors_dropped", 100.0))
                .is_empty(),
            "a substring match would have fired here — that was the old engine's bug"
        );
    }

    /// Was `test_alert_rule_evaluate`.
    #[test]
    fn every_operator_evaluates() {
        for (op, fires, quiet) in [
            (ComparisonOp::GreaterThan, 150.0, 100.0),
            (ComparisonOp::GreaterOrEqual, 100.0, 99.0),
            (ComparisonOp::LessThan, 50.0, 100.0),
            (ComparisonOp::LessOrEqual, 100.0, 101.0),
            (ComparisonOp::Equal, 100.0, 101.0),
            (ComparisonOp::NotEqual, 101.0, 100.0),
        ] {
            let e = evaluator(vec![ThresholdRule::new("r", "m", op, 100.0)]);
            assert!(
                firing_alert(&e.decide(&point("m", fires))).is_some(),
                "{op} should fire at {fires}"
            );
            let e = evaluator(vec![ThresholdRule::new("r", "m", op, 100.0)]);
            assert!(
                firing_alert(&e.decide(&point("m", quiet))).is_none(),
                "{op} should not fire at {quiet}"
            );
        }
    }

    /// Was `test_alerts_state_check_metric`, minus the cooldown.
    ///
    /// The GUI had a flat 60-second cooldown per `protocol/source/metric` —
    /// origin-blind, so two hosts with the same `source` shared one slot, and
    /// unrelated to whether the condition was still true. The reporter's
    /// `for` window and #929's recovery window replace it with something that
    /// means what it says.
    #[test]
    fn a_repeated_violation_is_one_alert_not_a_stream_of_them() {
        let e = evaluator(vec![rule()]);
        let first = firing_alert(&e.decide(&point("cpu/usage_percent", 150.0)))
            .unwrap()
            .alert_key();
        let again = firing_alert(&e.decide(&point("cpu/usage_percent", 200.0)))
            .unwrap()
            .alert_key();
        assert_eq!(
            first, again,
            "the same condition is the same alert; the reporter deduplicates it"
        );
        assert!(e.decide(&point("cpu/usage_percent", 50.0))[0].is_reconcile());
    }
}
