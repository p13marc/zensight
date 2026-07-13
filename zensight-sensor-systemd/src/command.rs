//! Sentinel control plane (#277): `@rpc/systemd/expectations/set` (hot-swap the rule
//! set) + `@rpc/systemd/expectations` (read reply of the current set). Mirrors
//! the netlink sentinel command channel.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zensight_common::command::{command_key, status_key};

use crate::sentinel::{ExpectationsConfig, SentinelHandle};

const EXPECTATIONS_TOPIC: &str = "expectations";

/// A runtime command on `@rpc/systemd/expectations/set`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExpectationCommand {
    /// Replace the entire expectation set (GUI authoring, #278).
    SetExpectations(ExpectationsConfig),
}

async fn apply(handle: &SentinelHandle, cmd: ExpectationCommand) {
    match cmd {
        ExpectationCommand::SetExpectations(cfg) => {
            tracing::info!("sentinel: SetExpectations applied");
            handle.replace(cfg).await;
        }
    }
}

/// Run the sentinel command/status channel until the session closes.
pub async fn run(session: Arc<zenoh::Session>, producer: String, handle: SentinelHandle) {
    let cmd_key = command_key(&producer, EXPECTATIONS_TOPIC);
    let stat_key = status_key(&producer, EXPECTATIONS_TOPIC);

    let subscriber = match session.declare_queryable(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "sentinel: subscribe to commands failed");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "sentinel: declare status queryable failed");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "sentinel: control channel ready");

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                match query {
                    Ok(query) => {
                        let payload = query
                            .payload()
                            .map(|p| p.to_bytes().to_vec())
                            .unwrap_or_default();
                        match serde_json::from_slice::<ExpectationCommand>(&payload) {
                            Ok(cmd) => {
                                apply(&handle, cmd).await;
                                if let Err(e) = query.reply(cmd_key.as_str(), Vec::<u8>::new()).await {
                                    tracing::warn!(error = %e, "sentinel: ack failed");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "sentinel: bad expectation command");
                                let err = zensight_sensor_core::rpc::RpcError::invalid_args(e.to_string());
                                let _ = query
                                    .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sentinel: command queryable ended");
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
                                if let Err(e) = query.reply(stat_key.as_str(), payload).await {
                                    tracing::warn!(error = %e, "sentinel: reply to status query failed");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "sentinel: serialize status failed"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::ServiceActiveExpectation;

    #[test]
    fn set_expectations_command_json_shape() {
        // GUI sends `{"type":"set_expectations", ...flattened ExpectationsConfig}`.
        let json = r#"{"type":"set_expectations","forbid_failed":true,
            "services_active":[{"unit":"sshd.service"}]}"#;
        let cmd: ExpectationCommand = serde_json::from_str(json).unwrap();
        let ExpectationCommand::SetExpectations(cfg) = cmd;
        assert!(cfg.forbid_failed);
        assert_eq!(
            cfg.services_active,
            vec![ServiceActiveExpectation {
                unit: "sshd.service".into()
            }]
        );
    }
}
