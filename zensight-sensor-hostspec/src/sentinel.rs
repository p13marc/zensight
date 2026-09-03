//! The hostspec sentinel (#821): declarative host assertions, evaluated on a
//! tick, published as alerts — the systemd/netlink sentinel pattern (#277,
//! #278) for the things D-Bus and netlink cannot see.
//!
//! Design rules, inherited and kept:
//!
//! - **Pure checkers.** Every `check_*` takes an expectation and an
//!   observation and returns violations; no I/O, so the whole vocabulary is
//!   unit-tested from fixtures ([`crate::observe`] owns the I/O).
//! - **An unreadable observation is NOT satisfied.** A `stat()` refused by
//!   EPERM proves nothing; "could not check" must never render as "passed"
//!   (the systemd sentinel wrote this rule down first; RFC 09 §5.1 O4 is the
//!   same rule one layer up). Unreadable violations carry
//!   `check = unreadable` so an operator can tell them from real failures.
//! - **Named expectations.** The rule slug is `<kind>:<name>`
//!   (`mount:var-tmp-scratch`), per-expectation `severity` and `for_secs`
//!   override the set-wide defaults, and deleting an expectation resolves
//!   its alerts via the seen-rules diff (netlink's GC).
//! - **`default_for_secs = 0`.** Host state does not flap the way sockets
//!   do — a chmodded secret is wrong on the first sweep, and a debounce
//!   would only delay the page. The flappy cases (a listener during a
//!   service restart) get a per-expectation `for_secs`.
//! - **The vocabulary is closed and executes nothing.** No command/run/
//!   binary assertion exists, deliberately (#821: the user call): hostspec
//!   is strictly read-only.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock};

use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

// The wire types live in zensight-common (#816): three consumers — this
// sensor, the GUI's authoring form, and the @desired fleet author — and the
// RFC 08 §7 schema gate all need them there. Re-exported so in-crate paths
// (and the e2e) read unchanged.
pub use zensight_common::hostspec::{
    AbsentExpectation, AssertionResult, AssertionStatus, ContentExpectation, ExpectationsConfig,
    FileExpectation, HostspecEvaluation, ListeningExpectation, MountExpectation, PermsExpectation,
    SymlinkExpectation, parse_mode,
};

use crate::observe::{
    self, FileFacts, IdTables, ListenEntry, MountEntry, Observation, containing_mount,
    visible_mount,
};

/// One failed clause of one assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub summary: String,
    /// `check` (which clause), `path`/`port`+`addr` (the subject), and
    /// `expected`/`actual` where they exist — capped, an alert label is a
    /// discriminator, not a document.
    pub labels: Vec<(String, String)>,
}

fn cap(v: impl Into<String>) -> String {
    let mut v = v.into();
    if v.len() > 120 {
        v.truncate(120);
        v.push('…');
    }
    v
}

fn violation(check: &str, subject: (&str, &str), summary: String) -> Violation {
    Violation {
        summary,
        labels: vec![
            ("check".into(), check.into()),
            (subject.0.into(), subject.1.into()),
        ],
    }
}

fn unreadable(subject: (&str, &str), what: &str, err: &str) -> Violation {
    Violation {
        summary: format!("unreadable: cannot check {what}: {err}"),
        labels: vec![
            ("check".into(), "unreadable".into()),
            (subject.0.into(), subject.1.into()),
            ("actual".into(), cap(err)),
        ],
    }
}

// ---------------------------------------------------------------------------
// The assertion vocabulary — seven kinds, closed.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pure checkers — one per kind. Fixture in, violations out.
// ---------------------------------------------------------------------------

pub fn check_mount(
    exp: &MountExpectation,
    mounts: &Observation<Vec<MountEntry>>,
) -> Vec<Violation> {
    let subj = ("path", exp.path.as_str());
    let mounts = match mounts {
        Observation::Present(m) => m,
        Observation::Absent => {
            return vec![unreadable(subj, "mounts", "/proc/self/mountinfo missing")];
        }
        Observation::Unreadable(e) => return vec![unreadable(subj, "mounts", e)],
    };
    let Some(vis) = visible_mount(mounts, &exp.path) else {
        return vec![violation(
            "mount",
            subj,
            format!("expected {} to be a mount point, and it is not", exp.path),
        )];
    };
    let mut out = Vec::new();
    if let Some(fstype) = &exp.fstype
        && &vis.fstype != fstype
    {
        let mut v = violation(
            "fstype",
            subj,
            format!(
                "expected {} mounted as {fstype}, found {}",
                exp.path, vis.fstype
            ),
        );
        v.labels.push(("expected".into(), cap(fstype)));
        v.labels.push(("actual".into(), cap(&vis.fstype)));
        out.push(v);
    }
    for opt in &exp.options {
        if !vis.mount_opts.contains(opt) && !vis.super_opts.contains(opt) {
            let mut v = violation(
                "options",
                subj,
                format!("expected {} mounted with {opt}, and it is not", exp.path),
            );
            v.labels.push(("expected".into(), cap(opt)));
            v.labels
                .push(("actual".into(), cap(vis.mount_opts.join(","))));
            out.push(v);
        }
    }
    if let Some(src) = &exp.is_bind_of {
        // What would a bind of `src` look like? Same device as the mount
        // containing src, with root = containing.root ++ (src − containing
        // .mount_point). Computed, never assumed "/": btrfs subvolume roots
        // make plain mounts carry non-"/" roots too.
        match containing_mount(mounts, src) {
            None => out.push(violation(
                "bind",
                subj,
                format!("cannot resolve {src}: no containing mount"),
            )),
            Some(cont) => {
                let rel = &src[cont.mount_point.len()..];
                let expected_root = if cont.root == "/" {
                    if rel.is_empty() {
                        "/".to_string()
                    } else {
                        rel.to_string()
                    }
                } else {
                    format!("{}{}", cont.root, rel)
                };
                if vis.dev != cont.dev || vis.root != expected_root {
                    let mut v = violation(
                        "bind",
                        subj,
                        format!(
                            "expected {} to be a bind of {src} (dev {}:{} root {expected_root}), \
                             found dev {}:{} root {}",
                            exp.path, cont.dev.0, cont.dev.1, vis.dev.0, vis.dev.1, vis.root
                        ),
                    );
                    v.labels.push(("expected".into(), cap(src)));
                    v.labels
                        .push(("actual".into(), cap(format!("{}:{}", vis.source, vis.root))));
                    out.push(v);
                }
            }
        }
    }
    out
}

/// Returns the violations and the advanced baseline (the size to latch when
/// this sweep PASSED the size clause; `None` leaves the stored baseline).
pub fn check_file(
    exp: &FileExpectation,
    obs: &Observation<FileFacts>,
    baseline: Option<u64>,
    now_unix: i64,
) -> (Vec<Violation>, Option<u64>) {
    let subj = ("path", exp.path.as_str());
    match obs {
        Observation::Unreadable(e) => (vec![unreadable(subj, "file", e)], None),
        Observation::Absent => {
            if exp.exists {
                (
                    vec![violation(
                        "exists",
                        subj,
                        format!("expected file {} to exist, and it does not", exp.path),
                    )],
                    None,
                )
            } else {
                (Vec::new(), None)
            }
        }
        Observation::Present(f) => {
            let mut out = Vec::new();
            if let Some(max_age) = exp.newer_than_secs {
                // A future mtime (clock skew) saturates to age 0 — a pass.
                let age = (now_unix - f.mtime_unix).max(0) as u64;
                if age > max_age {
                    let mut v = violation(
                        "mtime",
                        subj,
                        format!(
                            "expected {} newer than {max_age}s, last written {age}s ago",
                            exp.path
                        ),
                    );
                    v.labels.push(("expected".into(), format!("<= {max_age}s")));
                    v.labels.push(("actual".into(), format!("{age}s")));
                    out.push(v);
                }
            }
            let mut advanced = None;
            if let Some(pct) = exp.size_within_pct_of_previous {
                match baseline {
                    // First observation seeds the baseline without judging —
                    // there is no "previous" to drift from yet.
                    None => advanced = Some(f.size),
                    Some(0) => advanced = Some(f.size),
                    Some(base) => {
                        let drift = ((f.size as f64 - base as f64).abs() / base as f64) * 100.0;
                        if drift > pct {
                            let mut v = violation(
                                "size",
                                subj,
                                format!(
                                    "expected {} within {pct}% of previous size {base}, \
                                     found {} ({drift:.0}% drift)",
                                    exp.path, f.size
                                ),
                            );
                            v.labels
                                .push(("expected".into(), format!("{base} ±{pct}%")));
                            v.labels.push(("actual".into(), f.size.to_string()));
                            out.push(v);
                        } else {
                            advanced = Some(f.size);
                        }
                    }
                }
            }
            (out, advanced)
        }
    }
}

pub fn check_listening(
    exp: &ListeningExpectation,
    listeners: &Observation<Vec<ListenEntry>>,
) -> Vec<Violation> {
    let port_s = exp.port.to_string();
    let subj = ("port", port_s.as_str());
    let listeners = match listeners {
        Observation::Present(l) => l,
        Observation::Absent => return vec![unreadable(subj, "listeners", "/proc/net/tcp missing")],
        Observation::Unreadable(e) => return vec![unreadable(subj, "listeners", e)],
    };
    let want_addr: Option<std::net::IpAddr> = exp.addr.as_deref().and_then(|a| a.parse().ok());
    let matched = listeners.iter().any(|l| {
        l.port == exp.port
            && match want_addr {
                // `0.0.0.0` and `::` are distinct wildcards on purpose —
                // an exact comparison is what lets an operator forbid each.
                Some(a) => l.addr == a,
                None => true,
            }
    });
    let mk = |summary: String| {
        let mut v = violation(
            if exp.forbid { "no-listen" } else { "listen" },
            subj,
            summary,
        );
        if let Some(a) = &exp.addr {
            v.labels.push(("addr".into(), a.clone()));
        }
        vec![v]
    };
    match (matched, exp.forbid) {
        (true, true) => mk(format!(
            "expected NO listener on {}:{}, and one exists",
            exp.addr.as_deref().unwrap_or("*"),
            exp.port
        )),
        (false, false) => mk(format!(
            "expected a listener on {}:{}, and none exists",
            exp.addr.as_deref().unwrap_or("*"),
            exp.port
        )),
        _ => Vec::new(),
    }
}

pub fn check_symlink(exp: &SymlinkExpectation, obs: &Observation<FileFacts>) -> Vec<Violation> {
    let subj = ("path", exp.path.as_str());
    match obs {
        Observation::Unreadable(e) => vec![unreadable(subj, "symlink", e)],
        Observation::Absent => vec![violation(
            "target",
            subj,
            format!("expected symlink {} to exist, and it does not", exp.path),
        )],
        Observation::Present(f) => {
            if !f.is_symlink {
                return vec![violation(
                    "target",
                    subj,
                    format!("expected {} to be a symlink, and it is not", exp.path),
                )];
            }
            match &f.symlink_target {
                Some(t) if *t == exp.target => Vec::new(),
                Some(t) => {
                    let mut v = violation(
                        "target",
                        subj,
                        format!("expected {} -> {}, found -> {t}", exp.path, exp.target),
                    );
                    v.labels.push(("expected".into(), cap(&exp.target)));
                    v.labels.push(("actual".into(), cap(t)));
                    vec![v]
                }
                None => vec![unreadable(subj, "symlink target", "readlink failed")],
            }
        }
    }
}

pub fn check_absent(exp: &AbsentExpectation, obs: &Observation<FileFacts>) -> Vec<Violation> {
    let subj = ("path", exp.path.as_str());
    match obs {
        Observation::Absent => Vec::new(),
        Observation::Present(_) => vec![violation(
            "present",
            subj,
            format!("expected {} to be absent, and it exists", exp.path),
        )],
        // EPERM cannot PROVE absence — an unreadable parent may hide the
        // file. Not a pass.
        Observation::Unreadable(e) => vec![unreadable(subj, "absence", e)],
    }
}

pub fn check_content(exp: &ContentExpectation, obs: &Observation<String>) -> Vec<Violation> {
    let subj = ("path", exp.path.as_str());
    match obs {
        Observation::Unreadable(e) => vec![unreadable(subj, "content", e)],
        Observation::Absent => vec![violation(
            "contains",
            subj,
            format!("expected {} to exist, and it does not", exp.path),
        )],
        Observation::Present(text) => {
            let mut out = Vec::new();
            for needle in &exp.contains {
                if !text.contains(needle.as_str()) {
                    let mut v = violation(
                        "contains",
                        subj,
                        format!("expected {} to contain {:?}", exp.path, cap(needle)),
                    );
                    v.labels.push(("expected".into(), cap(needle)));
                    out.push(v);
                }
            }
            for pattern in &exp.matches {
                // validate() proved these compile; a failure here means the
                // set bypassed validation, which is worth the loud unreadable.
                match regex::Regex::new(pattern) {
                    Ok(re) if re.is_match(text) => {}
                    Ok(_) => {
                        let mut v = violation(
                            "matches",
                            subj,
                            format!("expected {} to match /{}/", exp.path, cap(pattern)),
                        );
                        v.labels.push(("expected".into(), cap(pattern)));
                        out.push(v);
                    }
                    Err(e) => out.push(unreadable(subj, "regex", &e.to_string())),
                }
            }
            out
        }
    }
}

pub fn check_perms(
    exp: &PermsExpectation,
    obs: &Observation<FileFacts>,
    ids: &IdTables,
) -> Vec<Violation> {
    let subj = ("path", exp.path.as_str());
    match obs {
        Observation::Unreadable(e) => vec![unreadable(subj, "permissions", e)],
        Observation::Absent => vec![violation(
            "exists",
            subj,
            format!("expected {} to exist, and it does not", exp.path),
        )],
        Observation::Present(f) => {
            let mut out = Vec::new();
            if let Some(mode) = exp.mode.as_deref().and_then(parse_mode)
                && f.mode != mode
            {
                let mut v = violation(
                    "mode",
                    subj,
                    format!(
                        "expected {} mode {:04o}, found {:04o}",
                        exp.path, mode, f.mode
                    ),
                );
                v.labels.push(("expected".into(), format!("{mode:04o}")));
                v.labels.push(("actual".into(), format!("{:04o}", f.mode)));
                out.push(v);
            }
            let check_id = |want: &str,
                            actual_id: u32,
                            table: &HashMap<u32, String>,
                            which: &str,
                            out: &mut Vec<Violation>| {
                let ok = match want.parse::<u32>() {
                    Ok(id) => id == actual_id,
                    Err(_) => table.get(&actual_id).is_some_and(|n| n == want),
                };
                if !ok {
                    let actual = table
                        .get(&actual_id)
                        .map(|n| format!("{n} ({actual_id})"))
                        .unwrap_or_else(|| actual_id.to_string());
                    let mut v = violation(
                        which,
                        subj,
                        format!("expected {} {which} {want}, found {actual}", exp.path),
                    );
                    v.labels.push(("expected".into(), cap(want)));
                    v.labels.push(("actual".into(), cap(actual)));
                    out.push(v);
                }
            };
            if let Some(owner) = &exp.owner {
                check_id(owner, f.uid, &ids.users, "owner", &mut out);
            }
            if let Some(group) = &exp.group {
                check_id(group, f.gid, &ids.groups, "group", &mut out);
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// The `spec` reply: what this host is being held to.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The evaluator.
// ---------------------------------------------------------------------------

/// Shared, hot-swappable assertion set + the latest evaluation. Cloning is
/// cheap (Arcs).
#[derive(Clone)]
pub struct SentinelHandle {
    expectations: Arc<RwLock<ExpectationsConfig>>,
    evaluation: Arc<RwLock<HostspecEvaluation>>,
    wake: Arc<Notify>,
}

impl SentinelHandle {
    pub fn new(cfg: ExpectationsConfig) -> Self {
        Self {
            expectations: Arc::new(RwLock::new(cfg)),
            evaluation: Arc::new(RwLock::new(HostspecEvaluation::default())),
            wake: Arc::new(Notify::new()),
        }
    }

    /// Replace the live set (already validated by the caller) and nudge the
    /// evaluator, so "set then read `spec`" reflects the new set within one
    /// sweep rather than one interval.
    pub async fn replace(&self, cfg: ExpectationsConfig) {
        *self.expectations.write().await = cfg;
        self.wake.notify_one();
    }

    pub async fn snapshot(&self) -> ExpectationsConfig {
        self.expectations.read().await.clone()
    }

    pub async fn evaluation(&self) -> HostspecEvaluation {
        self.evaluation.read().await.clone()
    }
}

/// One observation pass worth of raw material, gathered once per sweep so
/// fifty file expectations do not read `/proc/self/mountinfo` fifty times.
struct SweepInputs {
    mounts: Observation<Vec<MountEntry>>,
    listeners: Observation<Vec<ListenEntry>>,
    ids: IdTables,
    now_unix: i64,
}

impl SweepInputs {
    fn live() -> Self {
        SweepInputs {
            mounts: observe::read_mounts(),
            listeners: observe::read_listeners(),
            ids: observe::read_id_tables(),
            now_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }
}

pub struct Evaluator {
    host: String,
    handle: SentinelHandle,
    reporter: Arc<AlertReporter>,
    publisher: zensight_sensor_core::Publisher,
    /// Latched size baselines per file rule (module doc on `FileExpectation`).
    baselines: Mutex<HashMap<String, u64>>,
    /// Rules that have reported at least once — the GC set that resolves
    /// alerts for expectations deleted by a hot-swap (netlink's pattern).
    seen_rules: Mutex<HashSet<String>>,
}

impl Evaluator {
    pub fn new(
        host: impl Into<String>,
        cfg: ExpectationsConfig,
        reporter: Arc<AlertReporter>,
        publisher: zensight_sensor_core::Publisher,
    ) -> Self {
        Evaluator {
            host: host.into(),
            handle: SentinelHandle::new(cfg),
            reporter,
            publisher,
            baselines: Mutex::new(HashMap::new()),
            seen_rules: Mutex::new(HashSet::new()),
        }
    }

    pub fn handle(&self) -> SentinelHandle {
        self.handle.clone()
    }

    /// Run forever. The interval is re-read every iteration so a hot-swap
    /// that changes `eval_interval_secs` retimes the loop (netlink got this
    /// right and systemd did not — copied from the one that did).
    pub async fn run(self) {
        loop {
            let interval = {
                self.handle
                    .expectations
                    .read()
                    .await
                    .eval_interval_secs
                    .max(1)
            };
            self.sweep(&SweepInputs::live()).await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                _ = self.handle.wake.notified() => {}
            }
        }
    }

    /// One full evaluation: every expectation against one observation pass,
    /// violations to the reporter, deleted rules resolved, the `spec`
    /// snapshot and the failing-count gauge updated.
    async fn sweep(&self, inputs: &SweepInputs) {
        let cfg = self.handle.expectations.read().await.clone();
        let mut results: Vec<AssertionResult> = Vec::new();
        // (rule, severity, for_secs, recover_after_secs, violations)
        type Reported = (
            String,
            AlertSeverity,
            Option<u64>,
            Option<u64>,
            Vec<Violation>,
        );
        let mut reported: Vec<Reported> = Vec::new();

        macro_rules! run_kind {
            ($kind:literal, $list:expr, $check:expr) => {
                for exp in $list {
                    let rule = format!(concat!($kind, ":{}"), exp.name);
                    let violations: Vec<Violation> = $check(exp, &rule);
                    let status = if violations.is_empty() {
                        AssertionStatus::Pass
                    } else if violations.iter().all(|v| {
                        v.labels
                            .iter()
                            .any(|(k, v)| k == "check" && v == "unreadable")
                    }) {
                        AssertionStatus::Unreadable
                    } else {
                        AssertionStatus::Fail
                    };
                    results.push(AssertionResult {
                        rule: rule.clone(),
                        kind: $kind.into(),
                        name: exp.name.clone(),
                        status,
                        detail: violations.first().map(|v| v.summary.clone()),
                        severity: exp.severity,
                    });
                    reported.push((
                        rule,
                        exp.severity,
                        Some(exp.for_secs.unwrap_or(cfg.default_for_secs)),
                        Some(
                            exp.recover_after_secs
                                .unwrap_or(cfg.default_recover_after_secs),
                        ),
                        violations,
                    ));
                }
            };
        }

        run_kind!("mount", &cfg.mounts, |e: &MountExpectation, _r: &String| {
            check_mount(e, &inputs.mounts)
        });
        {
            // Files carry the latched baseline through the sweep.
            let mut baselines = self.baselines.lock().await;
            for exp in &cfg.files {
                let rule = format!("file:{}", exp.name);
                let obs = observe::observe_path(&exp.path);
                let (violations, advanced) =
                    check_file(exp, &obs, baselines.get(&rule).copied(), inputs.now_unix);
                if let Some(size) = advanced {
                    baselines.insert(rule.clone(), size);
                }
                let status = if violations.is_empty() {
                    AssertionStatus::Pass
                } else if violations.iter().all(|v| {
                    v.labels
                        .iter()
                        .any(|(k, v)| k == "check" && v == "unreadable")
                }) {
                    AssertionStatus::Unreadable
                } else {
                    AssertionStatus::Fail
                };
                results.push(AssertionResult {
                    rule: rule.clone(),
                    kind: "file".into(),
                    name: exp.name.clone(),
                    status,
                    detail: violations.first().map(|v| v.summary.clone()),
                    severity: exp.severity,
                });
                reported.push((
                    rule,
                    exp.severity,
                    Some(exp.for_secs.unwrap_or(cfg.default_for_secs)),
                    Some(
                        exp.recover_after_secs
                            .unwrap_or(cfg.default_recover_after_secs),
                    ),
                    violations,
                ));
            }
            // Baselines for rules deleted by a hot-swap go with them.
            let live = cfg.rule_slugs();
            baselines.retain(|rule, _| live.contains(rule));
        }
        run_kind!(
            "listening",
            &cfg.listening,
            |e: &ListeningExpectation, _r: &String| check_listening(e, &inputs.listeners)
        );
        run_kind!(
            "symlink",
            &cfg.symlinks,
            |e: &SymlinkExpectation, _r: &String| {
                check_symlink(e, &observe::observe_path(&e.path))
            }
        );
        run_kind!(
            "absent",
            &cfg.absent,
            |e: &AbsentExpectation, _r: &String| {
                check_absent(e, &observe::observe_path(&e.path))
            }
        );
        run_kind!(
            "content",
            &cfg.content,
            |e: &ContentExpectation, _r: &String| {
                check_content(e, &observe::read_content_capped(&e.path))
            }
        );
        run_kind!("perms", &cfg.perms, |e: &PermsExpectation, _r: &String| {
            check_perms(e, &observe::observe_path(&e.path), &inputs.ids)
        });

        // Publish: violations fire (debounced per expectation), clean rules
        // reconcile, and rules deleted from the set resolve via the GC diff.
        let mut seen = self.seen_rules.lock().await;
        for (rule, severity, for_secs, recover_after_secs, violations) in reported {
            seen.insert(rule.clone());
            self.report(&rule, severity, for_secs, recover_after_secs, violations)
                .await;
        }
        let live = cfg.rule_slugs();
        let gone: Vec<String> = seen
            .iter()
            .filter(|r| !live.contains(*r))
            .cloned()
            .collect();
        for rule in gone {
            // Immediate, never held (#932): a recovery window says "wait, in
            // case it comes back", and a DELETED assertion is not coming back.
            if let Err(e) = self
                .reporter
                .reconcile_opts(&rule, &[], zensight_sensor_core::ReconcileOpts::immediate())
                .await
            {
                tracing::warn!(error = %e, rule = %rule, "hostspec: failed to resolve removed rule");
            }
            seen.remove(&rule);
        }
        drop(seen);

        // The gauge: published EVERY sweep, zero included — an empty set
        // reads as 0, never as silence (and it is what keeps the device
        // card and the family-coverage audit alive).
        let failing = results
            .iter()
            .filter(|r| r.status != AssertionStatus::Pass)
            .count();
        let point = crate::map::failing_point(&self.host, failing);
        if let Err(e) = self.publisher.publish("assertions/failing", &point).await {
            tracing::warn!(error = %e, "hostspec: failed to publish failing gauge");
        }

        *self.handle.evaluation.write().await = HostspecEvaluation {
            evaluated_at_ms: zensight_common::current_timestamp_millis(),
            eval_interval_secs: cfg.eval_interval_secs,
            assertions: results,
        };
    }

    async fn report(
        &self,
        rule: &str,
        severity: AlertSeverity,
        for_secs: Option<u64>,
        recover_after_secs: Option<u64>,
        violations: Vec<Violation>,
    ) {
        let for_duration = for_secs.map(Duration::from_secs);
        // `None` means "use the reporter's own recovery"; the sweep has
        // already resolved the per-assertion override against the set-wide
        // default, so this is always `Some` from there (#932).
        let opts = zensight_sensor_core::ReconcileOpts {
            recover_after: recover_after_secs.map(Duration::from_secs),
        };
        let mut firing_keys = Vec::new();
        for v in violations {
            let mut alert = Alert::new(
                &self.host,
                Protocol::Hostspec,
                AlertKind::Expectation,
                rule,
                severity,
                v.summary,
            );
            for (k, val) in v.labels {
                alert = alert.with_label(k, val);
            }
            firing_keys.push(alert.alert_key());
            if let Err(e) = self.reporter.observe(alert, for_duration).await {
                tracing::warn!(error = %e, "hostspec: failed to publish alert");
            }
        }
        if let Err(e) = self.reporter.reconcile_opts(rule, &firing_keys, opts).await {
            tracing::warn!(error = %e, "hostspec: failed to reconcile alerts");
        }
    }
}

/// Validate a whole assertion set — called at config load (refuse to start)
/// and before every hot-swap `replace` (refuse with `error/invalid-args`).
/// Collects every failure, each naming its expectation. Lives in the SENSOR
/// (the wire types moved to zensight-common in #816): validation needs the
/// regex engine, and every other consumer of the types trusts this gate over
/// the bus rather than compiling it in.
pub fn validate(cfg: &ExpectationsConfig) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    if cfg.eval_interval_secs == 0 {
        errs.push("eval_interval_secs must be >= 1".into());
    }
    // Owned names: a few allocations per validation, and no lifetime
    // sleight of hand. This ran through a `transmute` to `&'static str` for
    // a while to save them; a config gate runs a handful of times a day.
    let mut names: HashSet<(&'static str, String)> = HashSet::new();
    let mut check_name = |kind: &'static str, name: &str, errs: &mut Vec<String>| {
        if name.is_empty() {
            errs.push(format!("{kind}: an expectation has an empty name"));
        } else if !names.insert((kind, name.to_string())) {
            errs.push(format!(
                "{kind}:{name}: duplicate name — the rule slug would collide and \
                 cross-resolve alerts"
            ));
        }
    };
    let abs = |kind: &str, name: &str, field: &str, p: &str, errs: &mut Vec<String>| {
        if !p.starts_with('/') {
            errs.push(format!("{kind}:{name}: {field} {p:?} is not absolute"));
        }
    };
    for e in &cfg.mounts {
        check_name("mount", &e.name, &mut errs);
        abs("mount", &e.name, "path", &e.path, &mut errs);
        if let Some(src) = &e.is_bind_of {
            abs("mount", &e.name, "is_bind_of", src, &mut errs);
        }
        if e.is_bind_of.is_none() && e.fstype.is_none() && e.options.is_empty() {
            // A bare mount expectation still asserts "is a mount point";
            // that is a real claim, so nothing to reject.
        }
    }
    for e in &cfg.files {
        check_name("file", &e.name, &mut errs);
        abs("file", &e.name, "path", &e.path, &mut errs);
        if let Some(s) = e.newer_than_secs
            && s == 0
        {
            errs.push(format!("file:{}: newer_than_secs must be > 0", e.name));
        }
        if let Some(p) = e.size_within_pct_of_previous
            && p.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        {
            errs.push(format!(
                "file:{}: size_within_pct_of_previous must be > 0",
                e.name
            ));
        }
        if !e.exists && (e.newer_than_secs.is_some() || e.size_within_pct_of_previous.is_some()) {
            // Legal: the clauses apply when it exists. Nothing to reject.
        }
    }
    for e in &cfg.listening {
        check_name("listening", &e.name, &mut errs);
        if let Some(a) = &e.addr
            && a.parse::<std::net::IpAddr>().is_err()
        {
            errs.push(format!(
                "listening:{}: addr {a:?} is not an IP address",
                e.name
            ));
        }
    }
    for e in &cfg.symlinks {
        check_name("symlink", &e.name, &mut errs);
        abs("symlink", &e.name, "path", &e.path, &mut errs);
    }
    for e in &cfg.absent {
        check_name("absent", &e.name, &mut errs);
        abs("absent", &e.name, "path", &e.path, &mut errs);
    }
    for e in &cfg.content {
        check_name("content", &e.name, &mut errs);
        abs("content", &e.name, "path", &e.path, &mut errs);
        if e.contains.is_empty() && e.matches.is_empty() {
            errs.push(format!(
                "content:{}: neither contains nor matches — checks nothing",
                e.name
            ));
        }
        for m in &e.matches {
            if let Err(err) = regex::Regex::new(m) {
                errs.push(format!("content:{}: bad regex {m:?}: {err}", e.name));
            }
        }
    }
    for e in &cfg.perms {
        check_name("perms", &e.name, &mut errs);
        abs("perms", &e.name, "path", &e.path, &mut errs);
        if e.mode.is_none() && e.owner.is_none() && e.group.is_none() {
            errs.push(format!(
                "perms:{}: no mode, owner or group — checks nothing",
                e.name
            ));
        }
        if let Some(m) = &e.mode
            && parse_mode(m).is_none()
        {
            errs.push(format!(
                "perms:{}: mode {m:?} is not octal permission bits (e.g. \"0600\")",
                e.name
            ));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::parse_mountinfo;

    fn facts(mode: u32, uid: u32, gid: u32, size: u64, mtime: i64) -> FileFacts {
        FileFacts {
            is_symlink: false,
            is_dir: false,
            mode,
            uid,
            gid,
            size,
            mtime_unix: mtime,
            symlink_target: None,
        }
    }

    const MOUNTS: &str = "\
61 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,errors=remount-ro
104 61 8:16 / /scratch rw,noatime shared:44 - ext4 /dev/sdb rw
99 61 8:16 /tmp /var/tmp rw,noatime shared:40 - ext4 /dev/sdb rw
120 61 0:30 /@home /home rw,relatime shared:50 - btrfs /dev/sda3 rw,subvol=/@home";

    fn mounts() -> Observation<Vec<MountEntry>> {
        Observation::Present(parse_mountinfo(MOUNTS))
    }

    fn mount_exp(bind: Option<&str>) -> MountExpectation {
        MountExpectation {
            name: "vt".into(),
            path: "/var/tmp".into(),
            is_bind_of: bind.map(String::from),
            fstype: None,
            options: Vec::new(),
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }
    }

    /// The issue's motivating case both ways: /var/tmp really bound to
    /// /scratch/tmp passes; bound elsewhere (or a plain mount) fails with
    /// the computed expected/actual in the labels.
    #[test]
    fn mount_bind_dev_and_root_must_both_match() {
        assert!(check_mount(&mount_exp(Some("/scratch/tmp")), &mounts()).is_empty());

        let v = check_mount(&mount_exp(Some("/scratch/other")), &mounts());
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "bind")
        );

        // Not a mount point at all.
        let mut e = mount_exp(None);
        e.path = "/not/mounted".into();
        let v = check_mount(&e, &mounts());
        assert!(v[0].summary.contains("not"), "{}", v[0].summary);
    }

    /// btrfs subvolume roots: a bind of /home/user computes expected root
    /// /@home/user — "/" is never assumed.
    #[test]
    fn mount_bind_through_btrfs_subvol_root() {
        let mut e = mount_exp(Some("/home/user"));
        e.path = "/var/tmp".into();
        let v = check_mount(&e, &mounts());
        assert!(
            v[0].summary.contains("/@home/user"),
            "expected root computed through the subvol root: {}",
            v[0].summary
        );
    }

    #[test]
    fn mount_unreadable_is_a_violation_not_a_pass() {
        let v = check_mount(&mount_exp(None), &Observation::Unreadable("EPERM".into()));
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "unreadable")
        );
    }

    fn file_exp() -> FileExpectation {
        FileExpectation {
            name: "backup".into(),
            path: "/backup/db.dump".into(),
            exists: true,
            newer_than_secs: Some(100),
            size_within_pct_of_previous: Some(40.0),
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }
    }

    /// The latched baseline: first sight seeds without judging; a halved
    /// size fires AND the baseline does not advance, so it keeps firing next
    /// sweep (no self-resolve blip); recovery re-latches.
    #[test]
    fn file_size_baseline_latches_on_pass_only() {
        let e = file_exp();
        let now = 1000;
        // Seed.
        let (v, adv) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 1000, now)),
            None,
            now,
        );
        assert!(v.is_empty());
        assert_eq!(adv, Some(1000));
        // Halved: fires, baseline held.
        let (v, adv) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 500, now)),
            Some(1000),
            now,
        );
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "size")
        );
        assert_eq!(adv, None, "a failing size must not become the new baseline");
        // Still halved next sweep: still firing (same baseline).
        let (v, _) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 500, now)),
            Some(1000),
            now,
        );
        assert_eq!(v.len(), 1);
        // Recovered: passes and re-latches.
        let (v, adv) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 950, now)),
            Some(1000),
            now,
        );
        assert!(v.is_empty());
        assert_eq!(adv, Some(950));
    }

    #[test]
    fn file_mtime_age_and_future_saturation() {
        let e = file_exp();
        let now = 10_000;
        let (v, _) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 1, now - 500)),
            None,
            now,
        );
        assert!(v.iter().any(|v| {
            v.labels
                .iter()
                .any(|(k, val)| k == "check" && val == "mtime")
        }));
        // Clock skew: a future mtime saturates to age 0 — a pass.
        let (v, _) = check_file(
            &e,
            &Observation::Present(facts(0o644, 0, 0, 1, now + 500)),
            None,
            now,
        );
        assert!(v.iter().all(|v| !v.summary.contains("last written")));
    }

    #[test]
    fn file_unreadable_and_absent() {
        let e = file_exp();
        let (v, _) = check_file(&e, &Observation::Unreadable("EPERM".into()), None, 0);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "unreadable")
        );
        let (v, _) = check_file(&e, &Observation::Absent, None, 0);
        assert!(v[0].summary.contains("exist"));
    }

    fn listeners() -> Observation<Vec<ListenEntry>> {
        Observation::Present(vec![
            ListenEntry {
                addr: "10.8.0.1".parse().unwrap(),
                port: 8443,
            },
            ListenEntry {
                addr: "0.0.0.0".parse().unwrap(),
                port: 80,
            },
            ListenEntry {
                addr: "::".parse().unwrap(),
                port: 81,
            },
        ])
    }

    #[test]
    fn listening_require_and_forbid() {
        let mut e = ListeningExpectation {
            name: "vpn".into(),
            port: 8443,
            addr: Some("10.8.0.1".into()),
            forbid: false,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_listening(&e, &listeners()).is_empty());
        e.port = 8444;
        assert_eq!(check_listening(&e, &listeners()).len(), 1);

        // Forbid the v4 wildcard: fires for :80, not for :81 (`::` is a
        // DIFFERENT wildcard — the distinction is deliberate and documented).
        let forbid4 = ListeningExpectation {
            name: "no-any4".into(),
            port: 80,
            addr: Some("0.0.0.0".into()),
            forbid: true,
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        assert_eq!(check_listening(&forbid4, &listeners()).len(), 1);
        let forbid4_on_81 = ListeningExpectation {
            port: 81,
            ..forbid4.clone()
        };
        assert!(
            check_listening(&forbid4_on_81, &listeners()).is_empty(),
            ":: must not satisfy a 0.0.0.0 forbid — they are distinct wildcards"
        );
    }

    #[test]
    fn absent_eperm_cannot_prove_absence() {
        let e = AbsentExpectation {
            name: "left".into(),
            path: "/tmp/x".into(),
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(check_absent(&e, &Observation::Absent).is_empty());
        assert_eq!(
            check_absent(&e, &Observation::Present(facts(0, 0, 0, 0, 0))).len(),
            1
        );
        let v = check_absent(&e, &Observation::Unreadable("EPERM".into()));
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "unreadable")
        );
    }

    #[test]
    fn content_contains_and_matches() {
        let e = ContentExpectation {
            name: "hosts".into(),
            path: "/etc/hosts".into(),
            contains: vec!["10.0.0.5 registry.internal".into()],
            matches: vec![r"^127\.0\.0\.1\s+localhost".into()],
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        let good = "127.0.0.1  localhost\n10.0.0.5 registry.internal\n";
        assert!(check_content(&e, &Observation::Present(good.into())).is_empty());
        let bad = "127.0.0.1  localhost\n";
        let v = check_content(&e, &Observation::Present(bad.into()));
        assert_eq!(v.len(), 1);
        assert!(
            v[0].labels
                .iter()
                .any(|(k, val)| k == "check" && val == "contains")
        );
    }

    #[test]
    fn perms_mode_owner_group_by_name_and_id() {
        let mut ids = IdTables::default();
        ids.users.insert(998, "deploy".into());
        ids.groups.insert(998, "deploy".into());
        let e = PermsExpectation {
            name: "key".into(),
            path: "/etc/deploy/key.pem".into(),
            mode: Some("0600".into()),
            owner: Some("deploy".into()),
            group: Some("998".into()),
            severity: AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        };
        assert!(
            check_perms(
                &e,
                &Observation::Present(facts(0o600, 998, 998, 1, 0)),
                &ids
            )
            .is_empty()
        );
        let v = check_perms(&e, &Observation::Present(facts(0o644, 0, 998, 1, 0)), &ids);
        assert_eq!(v.len(), 2, "mode and owner both fail: {v:?}");
    }

    #[test]
    fn symlink_literal_target() {
        let e = SymlinkExpectation {
            name: "java".into(),
            path: "/etc/alternatives/java".into(),
            target: "/usr/lib/jvm/x/bin/java".into(),
            severity: AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        };
        let mut f = facts(0o777, 0, 0, 0, 0);
        f.is_symlink = true;
        f.symlink_target = Some("/usr/lib/jvm/x/bin/java".into());
        assert!(check_symlink(&e, &Observation::Present(f.clone())).is_empty());
        f.symlink_target = Some("/usr/lib/jvm/y/bin/java".into());
        assert_eq!(check_symlink(&e, &Observation::Present(f)).len(), 1);
        // A plain file where a symlink was asserted.
        assert_eq!(
            check_symlink(&e, &Observation::Present(facts(0o644, 0, 0, 0, 0))).len(),
            1
        );
    }

    #[test]
    fn validate_names_every_offender() {
        let cfg: ExpectationsConfig = serde_json::from_value(serde_json::json!({
            "content": [
                { "name": "bad-re", "path": "/etc/hosts", "matches": ["["] },
                { "name": "vacuous", "path": "/etc/hosts" },
            ],
            "perms": [
                { "name": "bad-mode", "path": "relative/path", "mode": "9999" },
                { "name": "nothing", "path": "/x" },
            ],
            "listening": [
                { "name": "bad-addr", "port": 1, "addr": "not-an-ip" }
            ],
            "files": [
                { "name": "dup", "path": "/a" },
                { "name": "dup", "path": "/b" }
            ],
        }))
        .unwrap();
        let err = validate(&cfg).unwrap_err();
        for needle in [
            "bad-re",
            "vacuous",
            "bad-mode",
            "not absolute",
            "nothing",
            "bad-addr",
            "duplicate",
        ] {
            assert!(err.contains(needle), "missing {needle:?} in: {err}");
        }
        // And the default config validates.
        validate(&ExpectationsConfig::default()).unwrap();
    }
}
