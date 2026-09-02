//! `@rpc/historian/timeline` (#908).
//!
//! The last of the four declared procedures to be built. Newest-first,
//! windowed, filtered by kind and origin, and paged by `after_uid` — the
//! `@rpc/logs/events` contract, because a timeline and a log tail are the same
//! shape of question and there is no reason for a caller to learn two.

use std::sync::Arc;

use zensight_common::history::{TimelineEntry, TimelineKind, TimelineReply};
use zensight_common::rpc::{RpcError, RpcRequest, RpcResult};

use crate::ingest::SharedStore;

/// Default page size, and the ceiling. A timeline page is for reading, not
/// for bulk export.
pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 2_000;

/// Serve `@rpc/historian/timeline`.
pub async fn serve_timeline(
    session: Arc<zenoh::Session>,
    ctx: zensight_sensor_core::v1::V1Context,
    store: SharedStore,
    historian: String,
) -> zensight_sensor_core::Result<tokio::task::JoinHandle<()>> {
    zensight_sensor_core::rpc::serve(session, &ctx, &["timeline"], move |req: RpcRequest| {
        let store = store.clone();
        let historian = historian.clone();
        async move { answer(&req, &store, historian).await }
    })
    .await
}

/// Parse `kinds=alert,event`. An unrecognised name is an error rather than a
/// silent omission: `kinds=alerts` (plural, and wrong) that quietly returned
/// everything would look like a filter that does not work, and one that
/// quietly returned nothing would look like an empty timeline.
fn parse_kinds(raw: Option<String>) -> Result<Vec<TimelineKind>, RpcError> {
    let Some(raw) = raw else {
        return Ok(Vec::new()); // empty means both
    };
    let mut out = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match name {
            "alert" => out.push(TimelineKind::Alert),
            "event" => out.push(TimelineKind::Event),
            other => {
                return Err(RpcError::invalid_args(format!(
                    "unknown timeline kind {other:?} — one of alert, event"
                )));
            }
        }
    }
    Ok(out)
}

async fn answer(req: &RpcRequest, store: &SharedStore, historian: String) -> RpcResult {
    let num = |k: &str| req.param(k).and_then(|v| v.parse::<i64>().ok());
    let from_ms = num("from").unwrap_or(i64::MIN);
    let to_ms = num("to").unwrap_or(i64::MAX);
    if to_ms < from_ms {
        return Err(RpcError::invalid_args(format!(
            "to ({to_ms}) precedes from ({from_ms})"
        )));
    }
    let kinds = parse_kinds(req.param("kinds"))?;
    let origin = req.param("origin").filter(|o| o != "*");
    let after_uid = req.param("after_uid");
    let limit = req
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);

    let handle = {
        let g = store.lock().unwrap_or_else(|e| e.into_inner());
        g.persistent()
    };
    // Memory-only: the timeline is on disk or it is nowhere. An empty reply
    // rather than an error, because "this historian is not durable" is a
    // deployment fact the caller can see in `stats`, not a failed query.
    let Some(h) = handle else {
        return encode(TimelineReply {
            historian,
            entries: Vec::new(),
            next_cursor: None,
        });
    };

    let rows = tokio::task::spawn_blocking(move || {
        h.query_timeline(
            from_ms,
            to_ms,
            &kinds,
            origin.as_deref(),
            after_uid.as_deref(),
            limit,
        )
    })
    .await
    .map_err(|e| RpcError::new("error/historian/timeline", format!("task failed: {e}")))?
    .map_err(|e| RpcError::new("error/historian/timeline", format!("read failed: {e}")))?;

    // A full page means there may be more; a short one is the end. The cursor
    // is the last (oldest) uid returned, which is what the caller passes back
    // as `after_uid` — the same contract `@rpc/logs/events` has, so a reader
    // that can page one can page the other.
    let next_cursor = (rows.len() == limit)
        .then(|| rows.last().map(|r| r.uid.clone()))
        .flatten();

    encode(TimelineReply {
        historian,
        entries: rows
            .into_iter()
            .map(|r| TimelineEntry {
                uid: r.uid,
                ts: r.ts,
                kind: r.kind,
                origin: r.origin,
                key: r.key,
                active: r.active,
                summary: r.summary,
            })
            .collect(),
        next_cursor,
    })
}

fn encode(reply: TimelineReply) -> RpcResult {
    serde_json::to_vec(&reply)
        .map_err(|e| RpcError::new("error/historian/timeline", format!("encode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_and_an_unknown_one_is_refused() {
        assert_eq!(parse_kinds(None).unwrap(), vec![]);
        assert_eq!(
            parse_kinds(Some("alert,event".into())).unwrap(),
            vec![TimelineKind::Alert, TimelineKind::Event]
        );
        assert_eq!(
            parse_kinds(Some(" alert ".into())).unwrap(),
            vec![TimelineKind::Alert]
        );
        // `kinds=alerts` is the mistake this catches. Returning everything
        // would look like a filter that does not work; returning nothing would
        // look like an empty timeline. Both are worse than being told.
        let e = parse_kinds(Some("alerts".into())).expect_err("plural must be refused");
        assert_eq!(e.error, zensight_common::rpc::ERR_INVALID_ARGS);
        assert!(e.message.contains("alerts"));
    }
}
