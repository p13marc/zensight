//! Runtime stream-control channel: the GUI opens/closes streams over Zenoh.
//!
//! - commands (pub/sub): `zensight/parallax/<source>/@/commands/stream`
//!   carrying `Command<StreamControl>` (JSON or CBOR, sniffed)
//! - status (queryable): `zensight/parallax/<source>/@/status/streams`
//!   replying `Vec<StreamStatus>` (transitions are additionally *published*
//!   on the same key by the session actor)

use std::sync::Arc;

use zensight_common::command::{Command, command_key, status_key};
use zensight_common::decode_auto;
use zensight_common::stream::StreamControl;

use crate::session::{SessionHandle, SessionMsg};

/// Topic for the stream control surface.
pub const STREAM_TOPIC: &str = "stream";
/// Topic for the streams status surface.
pub const STREAMS_TOPIC: &str = "streams";

/// Run the stream command subscriber + status queryable until session close.
pub async fn run(session: Arc<zenoh::Session>, host_prefix: String, handle: SessionHandle) {
    let cmd_key = command_key(&host_prefix, STREAM_TOPIC);
    let stat_key = status_key(&host_prefix, STREAMS_TOPIC);

    let subscriber = match session.declare_subscriber(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "stream: failed to subscribe to commands");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "stream: failed to declare status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "stream: control channel ready");

    loop {
        tokio::select! {
            sample = subscriber.recv_async() => {
                match sample {
                    Ok(sample) => {
                        let payload = sample.payload().to_bytes();
                        match decode_auto::<Command<StreamControl>>(&payload) {
                            Ok(cmd) => {
                                tracing::debug!(command = ?cmd.body, "stream: control command");
                                handle.send(SessionMsg::Control(cmd.body)).await;
                            }
                            Err(e) => tracing::warn!(error = %e, "stream: bad control command"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stream: command subscriber ended");
                        return;
                    }
                }
            }
            query = queryable.recv_async() => {
                match query {
                    Ok(query) => {
                        let statuses = handle.statuses().await;
                        match serde_json::to_vec(&statuses) {
                            Ok(payload) => {
                                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                                    tracing::warn!(error = %e, "stream: failed to reply to status query");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "stream: failed to serialize status"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stream: status queryable ended");
                        return;
                    }
                }
            }
        }
    }
}
