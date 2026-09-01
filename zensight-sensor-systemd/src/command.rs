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

/// Read an `expectations/set` body in either shape it can legitimately arrive
/// in (#849).
///
/// The registry has always declared this procedure's request as
/// `ExpectationsConfig` — the plain set, which is exactly what hostspec's
/// equivalent accepts and what `@desired` carries. This sensor only ever
/// accepted the tagged `{"type":"set_expectations", …}` envelope the GUI
/// happens to send, so a fleet tool that built its body from `describe` was
/// refused by the one sensor that had told it what to send.
///
/// Rather than rename the request type to match the accident — a breaking
/// change to a shipped path, for a payload whose bytes do not change — the
/// declared shape is now accepted as well. The tagged form keeps working, so
/// no existing caller moves.
fn parse_set_body(payload: &[u8]) -> Result<ExpectationsConfig, serde_json::Error> {
    match serde_json::from_slice::<ExpectationCommand>(payload) {
        Ok(ExpectationCommand::SetExpectations(cfg)) => Ok(cfg),
        // The tagged parse fails on a plain set (no `type`); the plain parse
        // is the registry's declared shape, so its error is the one to
        // report — a caller sending neither gets told what the field names
        // should have been.
        Err(_) => serde_json::from_slice::<ExpectationsConfig>(payload),
    }
}

/// Parse, then run the same gate the `@desired` reconciler runs
/// (`sentinel::validate`): a body that decodes but describes an inert or
/// impossible set is refused with the reason, and the previous good set keeps
/// running.
trait AndThenValidated {
    fn and_then_validated(self) -> Result<ExpectationsConfig, String>;
}

impl AndThenValidated for Result<ExpectationsConfig, serde_json::Error> {
    fn and_then_validated(self) -> Result<ExpectationsConfig, String> {
        let cfg = self.map_err(|e| e.to_string())?;
        crate::sentinel::validate(&cfg)?;
        Ok(cfg)
    }
}

/// Run the sentinel command/status channel until the session closes.
///
/// `marker` is the shared `applied/<topic>` marker (#849): the `@desired`
/// reconciler is the other writer to this same handle, and stamping
/// `source: rpc` here is what keeps the marker honest about who won last.
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    handle: SentinelHandle,
    marker: zensight_sensor_core::desired::AppliedMarker,
) {
    let cmd_key = command_key(&producer, EXPECTATIONS_TOPIC);
    let stat_key = status_key(&producer, EXPECTATIONS_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "sentinel: subscribe to commands failed");
            return;
        }
    };
    let queryable = match zensight_common::served::serve_queryable(&session, &stat_key).await {
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
                        match parse_set_body(&payload).and_then_validated() {
                            Ok(cfg) => {
                                tracing::info!("sentinel: expectation set replaced");
                                handle.replace(cfg.clone()).await;
                                marker
                                    .publish(
                                        zensight_common::desired::AppliedSource::Rpc,
                                        &cfg,
                                        None,
                                        None,
                                    )
                                    .await;
                                if let Err(e) = query.reply(cmd_key.as_str(), Vec::<u8>::new()).await {
                                    tracing::warn!(error = %e, "sentinel: ack failed");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "sentinel: bad expectation command");
                                let err = zensight_sensor_core::rpc::RpcError::invalid_args(e);
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

    /// Both shapes an `expectations/set` body can legitimately arrive in
    /// (#849). The tagged envelope is what the GUI has always sent; the plain
    /// set is what the registry has always *declared*, what hostspec accepts,
    /// and what `@desired` carries — so a fleet tool that built its body from
    /// `describe` used to be refused by the very sensor that told it what to
    /// send.
    #[test]
    fn a_set_body_is_read_in_either_shape() {
        let tagged = br#"{"type":"set_expectations","forbid_failed":true,
            "services_active":[{"unit":"sshd.service"}]}"#;
        let plain = br#"{"forbid_failed":true,
            "services_active":[{"unit":"sshd.service"}]}"#;

        let from_tagged = parse_set_body(tagged).expect("the GUI's envelope");
        let from_plain = parse_set_body(plain).expect("the registry's declared shape");
        assert_eq!(from_tagged, from_plain);
        assert!(from_plain.forbid_failed);
        assert_eq!(
            from_plain.services_active,
            vec![ServiceActiveExpectation {
                unit: "sshd.service".into()
            }]
        );

        // Neither shape: the refusal describes what was wrong with the body
        // as a PLAIN set — the shape `describe` advertises — rather than
        // complaining that a `type` tag the registry never mentions is
        // missing, which is what a caller following the registry would have
        // been told before.
        let err = parse_set_body(br#"{"nonsense": 1, "services_active": "not-a-list"}"#)
            .expect_err("a body that is neither shape must be refused");
        let msg = err.to_string();
        assert!(
            !msg.contains("missing field `type`"),
            "the refusal must be about the declared shape, not the envelope: {msg}"
        );
        assert!(msg.contains("expected a sequence"), "{msg}");
    }
}
