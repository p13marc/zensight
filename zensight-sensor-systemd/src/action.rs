//! Gated service control (#283) — **default OFF, opt-in only**.
//!
//! `@/commands/action` accepts `{verb, unit}` (start/stop/restart/reload). The
//! unit is validated against an allowlist, the corresponding `Manager` method is
//! called with `mode=replace`, and the async job is tracked to completion via the
//! `JobRemoved` signal. The outcome is published on `@/status/action` and written
//! to the audit log. Nothing is declared unless `actions.enabled` is set — a
//! disabled sensor is strictly read-only.
//!
//! Authorization is handled by systemd/polkit, not here: run as root, or
//! unprivileged with a scoped polkit rule granting
//! `org.freedesktop.systemd1.manage-units` for the allowlisted units (see
//! docs/SENSORS.md). The allowlist is defence-in-depth on top of polkit.

use std::sync::Arc;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use zensight_common::command::{command_key, status_key};

use crate::config::ActionsConfig;
use crate::dbus::ManagerProxy;

const ACTION_TOPIC: &str = "action";

/// A service-control verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Reload,
}

impl Verb {
    fn as_str(&self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Reload => "reload",
        }
    }
}

/// An action request on `@/commands/action`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionCommand {
    pub verb: Verb,
    pub unit: String,
}

/// The outcome of the most recent action, replied on `@/status/action`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionStatus {
    pub unit: String,
    pub verb: String,
    /// Whether the request passed validation and was issued.
    pub accepted: bool,
    /// The `JobRemoved` result (`done`/`failed`/`timeout`/`canceled`/…) when the
    /// job completed; `None` if rejected or still pending at reply time.
    #[serde(default)]
    pub result: Option<String>,
    /// Rejection / error reason when `accepted` is false or the job errored.
    #[serde(default)]
    pub error: Option<String>,
    pub ts_unix: i64,
}

/// Validate a request against the allowlist (pure — unit-testable). `Ok(())` when
/// the unit matches at least one allow pattern; `Err(reason)` otherwise.
pub fn validate(allow: &[glob::Pattern], unit: &str) -> Result<(), String> {
    if unit.is_empty() {
        return Err("empty unit".to_string());
    }
    if allow.iter().any(|p| p.matches(unit)) {
        Ok(())
    } else {
        Err(format!("unit {unit} not in actions.allow_units allowlist"))
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Run the gated action channel until the session closes. No-op (returns
/// immediately) unless actions are enabled.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, cfg: ActionsConfig) {
    if !cfg.enabled {
        tracing::info!("systemd service control disabled (actions.enabled = false)");
        return;
    }
    let allow = crate::config::compile_watch(&cfg.allow_units);
    if allow.is_empty() {
        tracing::warn!(
            "systemd actions enabled but allow_units is empty — every request will be rejected"
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

    let cmd_key = command_key(&key_prefix, ACTION_TOPIC);
    let stat_key = status_key(&key_prefix, ACTION_TOPIC);
    let subscriber = match session.declare_subscriber(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "action: subscribe failed");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "action: declare status failed");
            return;
        }
    };
    tracing::warn!(commands = %cmd_key, status = %stat_key, allow = ?cfg.allow_units,
        "systemd service control ENABLED (start/stop/restart/reload)");

    let mut last = ActionStatus::default();
    loop {
        tokio::select! {
            sample = subscriber.recv_async() => {
                let Ok(sample) = sample else { return };
                let payload = sample.payload().to_bytes();
                match serde_json::from_slice::<ActionCommand>(&payload) {
                    Ok(cmd) => {
                        last = execute(&manager, &allow, &cmd, cfg.job_timeout_secs).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "action: bad action command"),
                }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { return };
                match serde_json::to_vec(&last) {
                    Ok(payload) => {
                        if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                            tracing::warn!(error = %e, "action: status reply failed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "action: serialize status failed"),
                }
            }
        }
    }
}

/// Validate + issue one action, tracking the job to completion. Always audit-logs.
async fn execute(
    manager: &ManagerProxy<'_>,
    allow: &[glob::Pattern],
    cmd: &ActionCommand,
    job_timeout_secs: u64,
) -> ActionStatus {
    let verb = cmd.verb.as_str();
    // Allowlist check (defence-in-depth on top of polkit).
    if let Err(reason) = validate(allow, &cmd.unit) {
        tracing::warn!(target: "zensight::audit", %verb, unit = %cmd.unit, decision = "rejected",
            reason = %reason, "service action rejected");
        return ActionStatus {
            unit: cmd.unit.clone(),
            verb: verb.to_string(),
            accepted: false,
            result: None,
            error: Some(reason),
            ts_unix: now_unix(),
        };
    }

    // Start listening for JobRemoved BEFORE issuing, so we can't miss the signal.
    let job_stream = manager.receive_job_removed().await;

    let issued: zbus::Result<zbus::zvariant::OwnedObjectPath> = match cmd.verb {
        Verb::Start => manager.start_unit(&cmd.unit, "replace").await,
        Verb::Stop => manager.stop_unit(&cmd.unit, "replace").await,
        Verb::Restart => manager.restart_unit(&cmd.unit, "replace").await,
        Verb::Reload => manager.reload_unit(&cmd.unit, "replace").await,
    };
    let job_path = match issued {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "zensight::audit", %verb, unit = %cmd.unit, decision = "accepted",
                result = "error", error = %e, "service action failed to enqueue");
            return ActionStatus {
                unit: cmd.unit.clone(),
                verb: verb.to_string(),
                accepted: true,
                result: None,
                error: Some(e.to_string()),
                ts_unix: now_unix(),
            };
        }
    };

    // Track the job to completion via JobRemoved (bounded wait).
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
        verb: verb.to_string(),
        accepted: true,
        result,
        error: None,
        ts_unix: now_unix(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(patterns: &[&str]) -> Vec<glob::Pattern> {
        patterns
            .iter()
            .map(|p| glob::Pattern::new(p).unwrap())
            .collect()
    }

    #[test]
    fn allowlist_matches_globs() {
        let a = allow(&["nginx.service", "app-*.service"]);
        assert!(validate(&a, "nginx.service").is_ok());
        assert!(validate(&a, "app-web.service").is_ok());
        assert!(validate(&a, "sshd.service").is_err());
        assert!(validate(&a, "").is_err());
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        assert!(validate(&[], "nginx.service").is_err());
    }

    #[test]
    fn action_command_json_shape() {
        let cmd: ActionCommand =
            serde_json::from_str(r#"{"verb":"restart","unit":"nginx.service"}"#).unwrap();
        assert_eq!(cmd.verb, Verb::Restart);
        assert_eq!(cmd.unit, "nginx.service");
        assert_eq!(cmd.verb.as_str(), "restart");
    }

    #[test]
    fn all_verbs_roundtrip() {
        for (s, v) in [
            ("start", Verb::Start),
            ("stop", Verb::Stop),
            ("restart", Verb::Restart),
            ("reload", Verb::Reload),
        ] {
            let cmd: ActionCommand =
                serde_json::from_str(&format!(r#"{{"verb":"{s}","unit":"x.service"}}"#)).unwrap();
            assert_eq!(cmd.verb, v);
        }
    }
}
