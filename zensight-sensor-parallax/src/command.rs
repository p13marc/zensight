//! Runtime stream-control channel: the GUI opens/closes streams over Zenoh.
//!
//! v1 (`@rpc`, RFC 05; epic #453):
//! - write: `…/@rpc/parallax/stream/set` carrying `Command<StreamControl>`
//! - stream *status* is observable state (`state/parallax/stream/<s>`,
//!   published by the session actor); the `streams` read procedure serves
//!   the catalogue (query.rs).

use std::sync::Arc;

use zensight_common::command::{Command, command_key};
use zensight_common::decode_auto;
use zensight_common::stream::StreamControl;

use crate::session::{SessionHandle, SessionMsg};

/// Topic for the stream control surface.
pub const STREAM_TOPIC: &str = "stream";
/// Topic for the streams status surface.
pub const STREAMS_TOPIC: &str = "streams";

/// Run the stream command subscriber + status queryable until session close.
pub async fn run(session: Arc<zenoh::Session>, producer: String, handle: SessionHandle) {
    let cmd_key = command_key(&producer, STREAM_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "stream: failed to declare command queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, "stream: control channel ready");

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                match query {
                    Ok(query) => {
                        let payload = query
                            .payload()
                            .map(|p| p.to_bytes().to_vec())
                            .unwrap_or_default();
                        match decode_auto::<Command<StreamControl>>(&payload) {
                            Ok(cmd) => {
                                tracing::debug!(command = ?cmd.body, "stream: control command");
                                handle.send(SessionMsg::Control(cmd.body)).await;
                                if let Err(e) = query.reply(cmd_key.as_str(), Vec::<u8>::new()).await {
                                    tracing::warn!(error = %e, "stream: failed to ack command");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "stream: bad control command");
                                let err = zensight_sensor_core::rpc::RpcError::invalid_args(e.to_string());
                                let _ = query
                                    .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stream: command queryable ended");
                        return;
                    }
                }
            }
        }
    }
}
