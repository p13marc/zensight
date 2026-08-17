//! Gated service control (#283) — **default OFF, opt-in only**.
//!
//! `@rpc/systemd/action/set` accepts `{verb, unit}`. The request clears the
//! gates below, the corresponding `Manager` method is called, and the outcome is
//! replied, recorded on `@rpc/systemd/action` (most recent) and
//! `@rpc/systemd/actions` (a bounded ring), and written to the audit log.
//! Nothing writable is declared unless `actions.enabled` — a disabled sensor is
//! strictly read-only.
//!
//! `@rpc/systemd/action/capability` is the exception: it is served
//! **unconditionally**, replying `enabled: false` on a read-only host. Silence
//! is not a usable answer — it is emitted equally by a disabled gate, an offline
//! host, an older build, and a sensor busy with a long job — so a caller that
//! must decide whether to offer a control needs an affirmative "no". The probe
//! is read-only and names no units, so serving it creates no write surface.
//!
//! Authorization is handled by systemd/polkit, not here. The three verb classes
//! need three *different* polkit actions, which is why they have separate config
//! switches (see docs/units-and-actions.md):
//!
//! | Verbs | Manager method | polkit action |
//! |-------|----------------|---------------|
//! | start/stop/restart/reload | `StartUnit`… | `org.freedesktop.systemd1.manage-units` |
//! | enable/disable | `EnableUnitFiles`/`DisableUnitFiles` | `org.freedesktop.systemd1.manage-unit-files` |
//! | daemon-reload | `Reload` | `org.freedesktop.systemd1.reload-daemon` |
//!
//! The allowlist is defence-in-depth on top of polkit, not a substitute for it.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Mutex;
use zensight_common::action::{
    ActionCapability, ActionStatus, ServiceAction, UnitFileChange, Verb,
};
use zensight_common::command::{capability_key, command_key, query_key, status_key};

use crate::config::ActionsConfig;
use crate::dbus::ManagerProxy;

const ACTION_TOPIC: &str = "action";
/// The audit-timeline ring's procedure name (plural — `action` is the singular
/// "most recent").
const ACTIONS_TOPIC: &str = "actions";

/// Validate a request against the allowlist. `Ok(())` when the unit matches at
/// least one allow pattern; `Err(reason)` otherwise, phrased for the audit log
/// and the `error/gated` refusal.
///
/// Delegates the matching itself to [`zensight_common::action::allows`] so the
/// gate and the frontend's preview of the gate cannot disagree.
pub fn validate(allow: &[String], unit: &str) -> Result<(), String> {
    if unit.is_empty() {
        return Err("empty unit".to_string());
    }
    if zensight_common::action::allows(allow, unit) {
        Ok(())
    } else {
        Err(format!("unit {unit} not in actions.allow_units allowlist"))
    }
}

/// The gate state this configuration advertises. Pure — no bus, no D-Bus — so
/// the mapping from config to advertised capability is unit-testable.
pub fn capability(cfg: &ActionsConfig) -> ActionCapability {
    if !cfg.enabled {
        return ActionCapability::disabled(cfg.job_timeout_secs);
    }
    let mut verbs = vec![Verb::Start, Verb::Stop, Verb::Restart, Verb::Reload];
    if cfg.allow_unit_files {
        verbs.push(Verb::Enable);
        verbs.push(Verb::Disable);
    }
    if cfg.allow_daemon_reload {
        verbs.push(Verb::DaemonReload);
    }
    ActionCapability {
        enabled: true,
        allow_units: cfg.allow_units.clone(),
        job_timeout_secs: cfg.job_timeout_secs,
        verbs,
        unit_files: cfg.allow_unit_files,
        daemon_reload: cfg.allow_daemon_reload,
    }
}

/// Which gate rejected a request, if any. Pure, so the whole gate table is
/// testable without a bus or a system.
pub fn gate(cfg: &ActionsConfig, cmd: &ServiceAction) -> Result<(), String> {
    if !cfg.enabled {
        return Err("service control disabled (actions.enabled = false)".to_string());
    }
    if cmd.verb.writes_unit_files() && !cfg.allow_unit_files {
        return Err(format!(
            "{} requires actions.allow_unit_files (writes unit files, persists across reboots)",
            cmd.verb
        ));
    }
    if cmd.verb == Verb::DaemonReload {
        return if cfg.allow_daemon_reload {
            Ok(())
        } else {
            Err("daemon-reload requires actions.allow_daemon_reload".to_string())
        };
    }
    // Every remaining verb names a unit, so the allowlist applies. daemon-reload
    // returned above precisely because it is manager-wide and no unit glob can
    // scope it.
    validate(&cfg.allow_units, &cmd.unit)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn rejected(cmd: &ServiceAction, reason: String) -> ActionStatus {
    ActionStatus {
        unit: cmd.unit.clone(),
        verb: cmd.verb,
        accepted: false,
        result: None,
        error: Some(reason),
        changes: Vec::new(),
        needs_daemon_reload: false,
        ts_unix: now_unix(),
    }
}

/// The recorded outcomes: the most recent, and a bounded ring for the audit
/// timeline. Shared with the spawned per-request tasks.
#[derive(Clone)]
struct History {
    ring: Arc<Mutex<VecDeque<ActionStatus>>>,
    capacity: usize,
}

impl History {
    fn new(capacity: usize) -> Self {
        Self {
            ring: Arc::new(Mutex::new(VecDeque::new())),
            capacity: capacity.max(1),
        }
    }

    async fn record(&self, status: ActionStatus) {
        let mut ring = self.ring.lock().await;
        ring.push_front(status);
        while ring.len() > self.capacity {
            ring.pop_back();
        }
    }

    /// The most recent outcome, or `None` when nothing has run. Deliberately not
    /// a default-valued `ActionStatus`: an all-empty value is indistinguishable
    /// from a rejection.
    async fn last(&self) -> Option<ActionStatus> {
        self.ring.lock().await.front().cloned()
    }

    async fn recent(&self) -> Vec<ActionStatus> {
        self.ring.lock().await.iter().cloned().collect()
    }
}

/// Serve `@rpc/systemd/action/capability` until the session closes.
///
/// Runs as its own task with its own queryable — never as an arm of the action
/// loop. Sharing that loop would make the probe unanswerable for the duration of
/// a job (up to `job_timeout_secs`), which is exactly the silence the probe
/// exists to eliminate.
async fn serve_capability(session: Arc<zenoh::Session>, producer: String, cap: ActionCapability) {
    let key = capability_key(&producer, ACTION_TOPIC);
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "action: declare capability failed");
            return;
        }
    };
    tracing::info!(key = %key, enabled = cap.enabled, "systemd service-control probe ready");
    while let Ok(query) = queryable.recv_async().await {
        reply_json(&query, &key, &cap).await;
    }
}

/// Run the gated action channel until the session closes.
pub async fn run(session: Arc<zenoh::Session>, producer: String, cfg: ActionsConfig) {
    // Declared first and unconditionally: a read-only host must still be able to
    // say so out loud.
    tokio::spawn(serve_capability(
        session.clone(),
        producer.clone(),
        capability(&cfg),
    ));

    if !cfg.enabled {
        tracing::info!("systemd service control disabled (actions.enabled = false)");
        return;
    }
    if cfg.allow_units.is_empty() {
        tracing::warn!(
            "systemd actions enabled but allow_units is empty — every unit-scoped request will be rejected"
        );
    }

    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "action: system bus connect failed");
            return;
        }
    };
    let manager = match ManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "action: Manager proxy failed");
            return;
        }
    };
    // Enable JobRemoved so we can track job completion.
    if let Err(e) = manager.subscribe().await {
        tracing::warn!(error = %e, "action: Manager.Subscribe failed (job tracking degraded)");
    }

    let cmd_key = command_key(&producer, ACTION_TOPIC);
    let stat_key = status_key(&producer, ACTION_TOPIC);
    let ring_key = query_key(&producer, ACTIONS_TOPIC);
    let set_q = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "action: subscribe failed");
            return;
        }
    };
    let status_q = match zensight_common::served::serve_queryable(&session, &stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "action: declare status failed");
            return;
        }
    };
    let ring_q = match zensight_common::served::serve_queryable(&session, &ring_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %ring_key, "action: declare history failed");
            return;
        }
    };
    tracing::warn!(commands = %cmd_key, status = %stat_key, history = %ring_key,
        allow = ?cfg.allow_units, unit_files = cfg.allow_unit_files,
        daemon_reload = cfg.allow_daemon_reload,
        "systemd service control ENABLED");

    let history = History::new(cfg.history_capacity);
    loop {
        tokio::select! {
            query = set_q.recv_async() => {
                let Ok(query) = query else { return };
                let payload = query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default();
                let cmd = match serde_json::from_slice::<ServiceAction>(&payload) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        tracing::warn!(error = %e, "action: bad action command");
                        let err = zensight_sensor_core::rpc::RpcError::invalid_args(e.to_string());
                        let _ = query
                            .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                            .await;
                        continue;
                    }
                };
                // Gate before spawning: a refusal costs nothing and must be
                // audited even when the host is busy.
                if let Err(reason) = gate(&cfg, &cmd) {
                    tracing::warn!(target: "zensight::audit", verb = %cmd.verb, unit = %cmd.unit,
                        decision = "rejected", reason = %reason, "service action rejected");
                    history.record(rejected(&cmd, reason.clone())).await;
                    let err = zensight_sensor_core::rpc::RpcError::gated(reason);
                    let _ = query
                        .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                        .await;
                    continue;
                }
                // Run each accepted action on its own task. Awaiting `execute`
                // inline would block this loop — and therefore the status and
                // history reads — for the whole job, and would serialize two
                // operators acting on two different units of the same host.
                let conn = conn.clone();
                let history = history.clone();
                let timeout = cfg.job_timeout_secs;
                let cmd_key = cmd_key.clone();
                tokio::spawn(async move {
                    let status = match ManagerProxy::new(&conn).await {
                        Ok(manager) => execute(&manager, &cmd, timeout).await,
                        Err(e) => {
                            tracing::error!(error = %e, "action: Manager proxy failed");
                            rejected(&cmd, format!("D-Bus proxy failed: {e}"))
                        }
                    };
                    history.record(status.clone()).await;
                    match serde_json::to_vec(&status) {
                        Ok(body) => {
                            if let Err(e) = query.reply(cmd_key.as_str(), body).await {
                                tracing::warn!(error = %e, "action: reply failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "action: serialize outcome failed"),
                    }
                });
            }
            query = status_q.recv_async() => {
                let Ok(query) = query else { return };
                // `null` when nothing has run — not an empty ActionStatus, which
                // reads identically to a rejection.
                reply_json(&query, &stat_key, &history.last().await).await;
            }
            query = ring_q.recv_async() => {
                let Ok(query) = query else { return };
                reply_json(&query, &ring_key, &history.recent().await).await;
            }
        }
    }
}

/// Issue one already-gated action, tracking a job to completion where there is
/// one. Always audit-logs.
async fn execute(
    manager: &ManagerProxy<'_>,
    cmd: &ServiceAction,
    job_timeout_secs: u64,
) -> ActionStatus {
    if cmd.verb.enqueues_job() {
        return execute_job(manager, cmd, job_timeout_secs).await;
    }
    execute_immediate(manager, cmd).await
}

/// The four verbs that enqueue a systemd job: issue, then wait (bounded) for the
/// matching `JobRemoved`.
async fn execute_job(
    manager: &ManagerProxy<'_>,
    cmd: &ServiceAction,
    job_timeout_secs: u64,
) -> ActionStatus {
    let verb = cmd.verb;
    // Start listening for JobRemoved BEFORE issuing, so we can't miss the signal.
    let job_stream = manager.receive_job_removed().await;

    let issued = match verb {
        Verb::Start => manager.start_unit(&cmd.unit, "replace").await,
        Verb::Stop => manager.stop_unit(&cmd.unit, "replace").await,
        Verb::Restart => manager.restart_unit(&cmd.unit, "replace").await,
        Verb::Reload => manager.reload_unit(&cmd.unit, "replace").await,
        // execute() routes the jobless verbs elsewhere.
        other => unreachable!("{other} does not enqueue a job"),
    };
    let job_path = match issued {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "zensight::audit", %verb, unit = %cmd.unit, decision = "accepted",
                result = "error", error = %e, "service action failed to enqueue");
            return ActionStatus {
                unit: cmd.unit.clone(),
                verb,
                accepted: true,
                result: None,
                error: Some(e.to_string()),
                changes: Vec::new(),
                needs_daemon_reload: false,
                ts_unix: now_unix(),
            };
        }
    };

    let result = match job_stream {
        Ok(stream) => track_job(stream, &job_path, job_timeout_secs).await,
        Err(e) => {
            tracing::warn!(error = %e, "action: JobRemoved stream unavailable; result unknown");
            None
        }
    };

    tracing::warn!(target: "zensight::audit", %verb, unit = %cmd.unit, decision = "accepted",
        job = %job_path.as_str(), result = result.as_deref().unwrap_or("pending"),
        "service action issued");
    ActionStatus {
        unit: cmd.unit.clone(),
        verb,
        accepted: true,
        result,
        error: None,
        changes: Vec::new(),
        needs_daemon_reload: false,
        ts_unix: now_unix(),
    }
}

/// The verbs that enqueue no job: `enable`, `disable`, `daemon-reload`. The call
/// returning *is* the outcome, so there is nothing to track.
async fn execute_immediate(manager: &ManagerProxy<'_>, cmd: &ServiceAction) -> ActionStatus {
    let verb = cmd.verb;
    let unit = cmd.unit.clone();
    // `runtime: false` writes under /etc (persistent); `force: false` refuses to
    // clobber a conflicting symlink rather than silently replacing it.
    let outcome: Result<(Vec<UnitFileChange>, bool), zbus::Error> = match verb {
        Verb::Enable => manager
            .enable_unit_files(&[unit.as_str()], false, false)
            .await
            .map(|(_carries_install_info, changes)| (unit_file_changes(changes), true)),
        Verb::Disable => manager
            .disable_unit_files(&[unit.as_str()], false)
            .await
            .map(|changes| (unit_file_changes(changes), true)),
        Verb::DaemonReload => manager.reload().await.map(|()| (Vec::new(), false)),
        other => unreachable!("{other} enqueues a job"),
    };

    match outcome {
        Ok((changes, needs_daemon_reload)) => {
            tracing::warn!(target: "zensight::audit", %verb, %unit, decision = "accepted",
                result = "applied", changes = changes.len(), "service action applied");
            ActionStatus {
                unit,
                verb,
                accepted: true,
                result: Some("applied".to_string()),
                error: None,
                changes,
                needs_daemon_reload,
                ts_unix: now_unix(),
            }
        }
        Err(e) => {
            tracing::warn!(target: "zensight::audit", %verb, %unit, decision = "accepted",
                result = "error", error = %e, "service action failed");
            ActionStatus {
                unit,
                verb,
                accepted: true,
                result: None,
                error: Some(e.to_string()),
                changes: Vec::new(),
                needs_daemon_reload: false,
                ts_unix: now_unix(),
            }
        }
    }
}

/// Map the D-Bus `a(sss)` change list onto the wire type.
fn unit_file_changes(changes: Vec<crate::dbus::UnitFileChangeTuple>) -> Vec<UnitFileChange> {
    changes
        .into_iter()
        .map(|(change, path, destination)| UnitFileChange {
            change,
            path,
            destination,
        })
        .collect()
}

/// Wait (bounded) for the `JobRemoved` matching `job_path`, returning its result
/// string (`done`/`failed`/…), or `None` on timeout.
async fn track_job(
    mut stream: crate::dbus::JobRemovedStream,
    job_path: &zbus::zvariant::OwnedObjectPath,
    timeout_secs: u64,
) -> Option<String> {
    let wait = async {
        while let Some(signal) = stream.next().await {
            if let Ok(args) = signal.args()
                && args.job.as_str() == job_path.as_str()
            {
                return Some(args.result.to_string());
            }
        }
        None
    };
    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs.max(1)), wait)
        .await
        .ok()
        .flatten()
}

/// Reply on the queryable's own CONCRETE key (never the query's possibly
/// wildcard selector — RFC 05 §2.1).
async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, key: &str, body: &T) {
    match serde_json::to_vec(body) {
        Ok(payload) => {
            if let Err(e) = query.reply(key, payload).await {
                tracing::warn!(error = %e, key = %key, "action: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "action: serialize failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, allow: &[&str]) -> ActionsConfig {
        ActionsConfig {
            enabled,
            allow_units: allow.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn act(verb: Verb, unit: &str) -> ServiceAction {
        ServiceAction {
            verb,
            unit: unit.to_string(),
        }
    }

    #[test]
    fn validate_reports_a_reason_for_the_audit_log() {
        let allow = vec!["nginx.service".to_string()];
        assert!(validate(&allow, "nginx.service").is_ok());
        assert!(
            validate(&allow, "sshd.service")
                .unwrap_err()
                .contains("not in actions.allow_units")
        );
        assert_eq!(validate(&allow, "").unwrap_err(), "empty unit");
    }

    #[test]
    fn the_master_switch_beats_every_other_gate() {
        let c = cfg(false, &["*"]);
        for verb in Verb::all() {
            assert!(gate(&c, &act(verb, "nginx.service")).is_err());
        }
    }

    #[test]
    fn job_verbs_need_only_the_allowlist() {
        let c = cfg(true, &["nginx.service"]);
        assert!(gate(&c, &act(Verb::Restart, "nginx.service")).is_ok());
        assert!(gate(&c, &act(Verb::Reload, "nginx.service")).is_ok());
        assert!(gate(&c, &act(Verb::Stop, "sshd.service")).is_err());
    }

    /// enable/disable write symlinks that survive a reboot, so an allowlist match
    /// alone must not be enough.
    #[test]
    fn unit_file_verbs_need_their_own_switch_on_top_of_the_allowlist() {
        let mut c = cfg(true, &["nginx.service"]);
        let err = gate(&c, &act(Verb::Enable, "nginx.service")).unwrap_err();
        assert!(err.contains("allow_unit_files"), "{err}");

        c.allow_unit_files = true;
        assert!(gate(&c, &act(Verb::Enable, "nginx.service")).is_ok());
        assert!(gate(&c, &act(Verb::Disable, "nginx.service")).is_ok());
        // The allowlist still applies.
        assert!(gate(&c, &act(Verb::Enable, "sshd.service")).is_err());
    }

    /// daemon-reload is manager-wide: no unit glob can scope it, so it is gated
    /// only by its own switch — and a permissive allowlist must not grant it.
    #[test]
    fn daemon_reload_is_gated_by_its_own_switch_not_the_allowlist() {
        let mut c = cfg(true, &["*"]);
        assert!(gate(&c, &act(Verb::DaemonReload, "")).is_err());
        c.allow_daemon_reload = true;
        assert!(gate(&c, &act(Verb::DaemonReload, "")).is_ok());
        // …and it needs no unit, unlike every other verb.
        assert!(gate(&c, &act(Verb::Restart, "")).is_err());
    }

    #[test]
    fn capability_reflects_the_config() {
        let cap = capability(&ActionsConfig::default());
        assert!(!cap.enabled);
        assert!(cap.verbs.is_empty());
        assert!(cap.allow_units.is_empty());
        assert_eq!(cap.job_timeout_secs, 30);

        let mut c = cfg(true, &["app-*.service"]);
        c.allow_unit_files = true;
        let cap = capability(&c);
        assert!(cap.enabled);
        assert_eq!(cap.allow_units, vec!["app-*.service".to_string()]);
        assert!(cap.verbs.contains(&Verb::Restart));
        assert!(cap.verbs.contains(&Verb::Enable));
        assert!(
            !cap.verbs.contains(&Verb::DaemonReload),
            "daemon-reload stays off unless its own switch is set"
        );
    }

    /// The advertised capability must agree with the gate that actually runs —
    /// the whole point of publishing it is that a caller can trust the preview.
    #[test]
    fn advertised_capability_agrees_with_the_gate() {
        for (enabled, files, reload) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let mut c = cfg(enabled, &["nginx.service"]);
            c.allow_unit_files = files;
            c.allow_daemon_reload = reload;
            let cap = capability(&c);
            for verb in Verb::all() {
                let unit = if verb.targets_unit() {
                    "nginx.service"
                } else {
                    ""
                };
                assert_eq!(
                    cap.permits(verb),
                    gate(&c, &act(verb, unit)).is_ok(),
                    "{verb} disagreed (enabled={enabled} files={files} reload={reload})"
                );
            }
        }
    }

    /// The whole point of the probe: a host with service control **off** still
    /// answers, and answers "off" — while exposing no write surface at all.
    ///
    /// Both halves matter. Without the reply, a caller cannot tell a read-only
    /// host from an offline one and has to guess whether to offer controls.
    /// Without the second assertion, the probe could quietly have become a
    /// reason to declare the writable procedures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disabled_sensor_answers_the_probe_and_serves_no_write_surface() {
        // Unique prefix so parallel test runs don't cross-talk.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let producer = format!("test_{nanos}/systemd");

        // Multicast scouting OFF. A default-config session joins whatever mesh
        // it can reach — including a live fleet on the same host — so a test
        // that scouts is not a test, it is a participant (RFC 09 §0.1).
        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .expect("disable multicast scouting");
        let session = Arc::new(zenoh::open(config).await.expect("open zenoh session"));

        // Default config: actions disabled. `run` returns straight after
        // spawning the probe, so this needs no D-Bus and no privileges.
        run(session.clone(), producer.clone(), ActionsConfig::default()).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let replies = session
            .get(&capability_key(&producer, ACTION_TOPIC))
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get capability");
        let reply = replies.recv_async().await.expect("the probe must answer");
        let sample = reply.result().expect("ok reply");
        let cap: ActionCapability =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("decode ActionCapability");
        assert!(!cap.enabled, "a read-only host says so out loud");
        assert!(cap.verbs.is_empty());
        assert!(cap.allow_units.is_empty());

        // …and the writable procedure is not declared, so nobody answers it.
        let replies = session
            .get(&command_key(&producer, ACTION_TOPIC))
            .timeout(std::time::Duration::from_secs(1))
            .await
            .expect("get action/set");
        assert!(
            replies.recv_async().await.is_err(),
            "a disabled sensor must expose no write surface"
        );
    }

    #[tokio::test]
    async fn history_is_bounded_and_newest_first() {
        let h = History::new(2);
        assert!(h.last().await.is_none(), "nothing has run yet");
        for unit in ["a.service", "b.service", "c.service"] {
            h.record(rejected(&act(Verb::Start, unit), "nope".into()))
                .await;
        }
        let recent = h.recent().await;
        assert_eq!(recent.len(), 2, "ring is capped");
        assert_eq!(recent[0].unit, "c.service", "newest first");
        assert_eq!(h.last().await.unwrap().unit, "c.service");
    }
}
