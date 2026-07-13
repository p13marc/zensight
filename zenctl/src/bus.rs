//! The bus half: session setup and the one fan-in call helper.
//!
//! RFC 05 §2.1 makes fleet GETs a discipline, not a default — get it wrong and
//! the failure is silent (one reply instead of the fleet, and no error). So the
//! discipline lives here, in [`fleet_get`], and no subcommand issues a raw
//! `session.get()`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use zenoh::Session;
use zenoh::query::{ConsolidationMode, QueryTarget};

/// How a producer answered a procedure call.
pub enum Answer {
    /// A value reply — RFC 05 §3: "a reply always indicates success".
    Value(Vec<u8>),
    /// An error reply (`reply_err`), carrying the `{error, message}` envelope
    /// when it parses. RFC 05 §3: "an error always indicates failure".
    Error { name: String, message: String },
}

/// One host's answer, attributed to the origin that actually replied.
pub struct FleetAnswer {
    pub origin: String,
    pub answer: Answer,
}

/// Open a session for a read-only explorer.
///
/// RFC 09 §5: debug tools run *without* the session namespace and spell full
/// keys — "which is also the honest view of what is on the wire". So we never
/// set `namespace`, and every key this tool prints is the real one.
///
/// Scouting defaults to **off**. A bus explorer that multicast-scouts will join
/// whatever mesh it can find, which is how a throwaway session ends up
/// contaminating a live fleet; opt in explicitly with `--scouting` when you
/// mean it.
pub async fn open(connect: &[String], listen: &[String], scouting: bool) -> Result<Session> {
    let mut config = zenoh::Config::default();
    let json_list = |v: &[String]| {
        let items: Vec<String> = v.iter().map(|e| format!("{e:?}")).collect();
        format!("[{}]", items.join(","))
    };
    config
        .insert_json5("scouting/multicast/enabled", &scouting.to_string())
        .ok();
    if !connect.is_empty() {
        config
            .insert_json5("connect/endpoints", &json_list(connect))
            .ok();
    }
    if !listen.is_empty() {
        config
            .insert_json5("listen/endpoints", &json_list(listen))
            .ok();
    }
    zenoh::open(config)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to open Zenoh session")
}

/// Call a procedure and collect **every** reply, attributed by origin.
///
/// The three things RFC 05 §2.1 requires, in the one place they cannot be
/// forgotten:
///
/// 1. **target = All.** The default `BestMatching` short-circuits to a single
///    queryable the moment any matching one is declared `complete` — "one
///    storage config away from silently collapsing the fleet to one reply".
/// 2. **consolidation = None.** Default consolidation keeps one reply *per
///    reply key*; belt-and-braces against a producer that wrongly echoes the
///    wildcard selector instead of replying on its own concrete key.
/// 3. **Attribution by the reply's own key**, never by the key we asked on —
///    that is what makes `*`-origin fan-out legible.
///
/// Silence is deliberately *not* interpreted here (RFC 05 §3.1: "no reply" is
/// not one condition). Callers that need a verdict join this against the
/// liveliness roster; see `cmd::doctor`.
pub async fn fleet_get(
    session: &Session,
    key: &str,
    payload: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<FleetAnswer>> {
    let mut builder = session
        .get(key)
        .target(QueryTarget::All)
        .consolidation(ConsolidationMode::None)
        .timeout(timeout);
    if let Some(body) = payload {
        builder = builder.payload(body);
    }
    let replies = builder
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("query failed: {key}"))?;

    let mut out = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => {
                let origin = origin_of(sample.key_expr().as_str());
                out.push(FleetAnswer {
                    origin,
                    answer: Answer::Value(sample.payload().to_bytes().to_vec()),
                });
            }
            Err(err) => {
                // The error envelope is `{ "error": "<name>", "message": "…" }`
                // (RFC 05 §3), with reserved names like `error/not-found`. If it
                // does not parse we still surface the bytes — an unreadable
                // refusal is still a refusal.
                let bytes = err.payload().to_bytes().to_vec();
                let (name, message) = match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(v) => (
                        v.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("error/unparsed")
                            .to_string(),
                        v.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    Err(_) => (
                        "error/unparsed".to_string(),
                        String::from_utf8_lossy(&bytes).to_string(),
                    ),
                };
                // An error reply has no sample, so no concrete key to attribute
                // by; zenoh does not surface the responder here.
                out.push(FleetAnswer {
                    origin: "?".to_string(),
                    answer: Answer::Error { name, message },
                });
            }
        }
    }
    Ok(out)
}

/// The origin chunk of a wire key, via the grammar (never by index — RFC 03
/// §1.1: positions are relative to the configured base).
fn origin_of(key: &str) -> String {
    zensight_common::keyexpr::parse_wire_key(key)
        .map(|k| k.origin.chunk().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// The fleet-presence roster: who is up, and what they run.
///
/// RFC 04 §5 — a liveliness query on `zensight/v1/*/state/*/alive`. Zero
/// payload bytes: the token *key* is the record. `@catalog` is asked for by
/// name because `*` can never match a verbatim service origin (property D4).
pub async fn roster(session: &Session, timeout: Duration) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for expr in [
        zensight_common::all_liveliness_wildcard(),
        zensight_common::correlator_alive_key(),
    ] {
        let Ok(replies) = session.liveliness().get(&expr).timeout(timeout).await else {
            continue;
        };
        while let Ok(reply) = replies.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            let key = sample.key_expr().as_str();
            let Some(parsed) = zensight_common::keyexpr::parse_wire_key(key) else {
                continue;
            };
            let origin = parsed.origin.chunk().to_string();
            // `@catalog`'s token has no producer chunk — the service *is* the
            // producer. Everything else names its producer in position 5.
            let producer = parsed
                .producer
                .as_ref()
                .map(|p| p.chunk())
                .unwrap_or_else(|| origin.trim_start_matches('@').to_string());
            out.entry(origin).or_default().push(producer);
        }
    }
    for producers in out.values_mut() {
        producers.sort();
        producers.dedup();
    }
    Ok(out)
}
