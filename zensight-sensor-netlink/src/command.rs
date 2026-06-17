//! Runtime control channel for the sentinel: lets the GUI author/push
//! expectations over Zenoh.
//!
//! - commands (pub/sub): `zensight/netlink/@/commands/expectations`
//! - status (queryable): `zensight/netlink/@/status/expectations`

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zensight_common::{command_key, status_key};

use crate::sentinel::{
    ExpectationsConfig, LinkExpectation, NeighborExpectation, RouteExpectation, SentinelHandle,
    SocketExpectation,
};

/// Topic for the expectations control surface.
pub const EXPECTATIONS_TOPIC: &str = "expectations";

/// A command pushed to the sentinel from the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExpectationCommand {
    /// Replace the entire live expectation set.
    SetExpectations(ExpectationsConfig),
    /// Add (or replace by name) a socket expectation.
    AddSocket(SocketExpectation),
    /// Add (or replace by iface) a link expectation.
    AddLink(LinkExpectation),
    /// Add (or replace by ip) a neighbor expectation.
    AddNeighbor(NeighborExpectation),
    /// Add (or replace by name) a route expectation.
    AddRoute(RouteExpectation),
    /// Remove an expectation by rule slug.
    Remove { rule: String },
}

/// Apply a command to the live expectation set.
pub async fn apply(handle: &SentinelHandle, cmd: ExpectationCommand) {
    match cmd {
        ExpectationCommand::SetExpectations(cfg) => {
            tracing::info!("sentinel: replacing expectation set via command");
            handle.replace(cfg).await;
        }
        ExpectationCommand::AddSocket(exp) => {
            tracing::info!(name = %exp.name, "sentinel: add socket expectation");
            handle.add_socket(exp).await;
        }
        ExpectationCommand::AddLink(exp) => {
            tracing::info!(iface = %exp.iface, "sentinel: add link expectation");
            handle.add_link(exp).await;
        }
        ExpectationCommand::AddNeighbor(exp) => {
            tracing::info!(ip = %exp.ip, "sentinel: add neighbor expectation");
            handle.add_neighbor(exp).await;
        }
        ExpectationCommand::AddRoute(exp) => {
            tracing::info!(name = %exp.name, "sentinel: add route expectation");
            handle.add_route(exp).await;
        }
        ExpectationCommand::Remove { rule } => {
            tracing::info!(rule = %rule, "sentinel: remove expectation");
            handle.remove(&rule).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::{ExpectationsConfig, SocketExpectation};
    use zensight_common::AlertSeverity;

    fn sock(name: &str, port: u16) -> SocketExpectation {
        SocketExpectation {
            name: name.into(),
            listen: Some(port),
            established_to: None,
            min: 1,
            forbid_listen: None,
            severity: AlertSeverity::Critical,
            for_secs: None,
        }
    }

    #[tokio::test]
    async fn apply_set_add_remove() {
        let handle = SentinelHandle::new(ExpectationsConfig::default());

        // SetExpectations replaces wholesale.
        let mut cfg = ExpectationsConfig::default();
        cfg.sockets.push(sock("sshd", 22));
        apply(&handle, ExpectationCommand::SetExpectations(cfg)).await;
        assert_eq!(handle.snapshot().await.sockets.len(), 1);

        // AddSocket adds (and replaces by name).
        apply(&handle, ExpectationCommand::AddSocket(sock("db", 5432))).await;
        apply(&handle, ExpectationCommand::AddSocket(sock("sshd", 2222))).await; // replace
        let snap = handle.snapshot().await;
        assert_eq!(snap.sockets.len(), 2);
        assert_eq!(
            snap.sockets
                .iter()
                .find(|e| e.name == "sshd")
                .unwrap()
                .listen,
            Some(2222)
        );

        // Remove by rule slug.
        apply(
            &handle,
            ExpectationCommand::Remove {
                rule: "socket:db".into(),
            },
        )
        .await;
        assert_eq!(handle.snapshot().await.sockets.len(), 1);
    }

    #[test]
    fn command_json_roundtrip() {
        let cmd = ExpectationCommand::AddSocket(sock("sshd", 22));
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("add_socket"));
        let back: ExpectationCommand = serde_json::from_str(&json).unwrap();
        matches!(back, ExpectationCommand::AddSocket(_));
    }
}

/// Run the command subscriber + status queryable until the session closes.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, handle: SentinelHandle) {
    let cmd_key = command_key(&key_prefix, EXPECTATIONS_TOPIC);
    let stat_key = status_key(&key_prefix, EXPECTATIONS_TOPIC);

    let subscriber = match session.declare_subscriber(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "sentinel: failed to subscribe to commands");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "sentinel: failed to declare status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "sentinel: control channel ready");

    loop {
        tokio::select! {
            sample = subscriber.recv_async() => {
                match sample {
                    Ok(sample) => {
                        let payload = sample.payload().to_bytes();
                        match serde_json::from_slice::<ExpectationCommand>(&payload) {
                            Ok(cmd) => apply(&handle, cmd).await,
                            Err(e) => tracing::warn!(error = %e, "sentinel: bad expectation command"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sentinel: command subscriber ended");
                        return;
                    }
                }
            }
            query = queryable.recv_async() => {
                match query {
                    Ok(query) => {
                        let snapshot = handle.snapshot().await;
                        match serde_json::to_vec(&snapshot) {
                            Ok(payload) => {
                                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                                    tracing::warn!(error = %e, "sentinel: failed to reply to status query");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "sentinel: failed to serialize status"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sentinel: status queryable ended");
                        return;
                    }
                }
            }
        }
    }
}
