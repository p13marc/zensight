//! Generic read-procedure calls (#1261, design §5.5).
//!
//! Every on-demand panel used to be a pair of messages — `FetchX` /
//! `XReceived` — a typed fetch function, a `*DetailState` field to land in
//! and an `update` arm per producer: forty pairs by the time this module was
//! written, and a sensor the GUI had not been compiled with could not be
//! asked anything at all. The wire never needed any of it: a read procedure
//! is a GET on `@rpc/<producer>/<procedure>?<params>`, its reply is a value
//! the producer's `describe` names the schema of, and a bounded reply says
//! what it is in the RFC 05 §3.2 envelope. So the GUI asks with one message
//! ([`Message::Call`](crate::message::Message::Call)), lands the answer with
//! one ([`Message::Reply`](crate::message::Message::Reply)), and keeps it
//! here, keyed by procedure, as a JSON value a bespoke view decodes when it
//! wants a type and the default view renders as it is.
//!
//! **What this is not.** Not a write path: a write procedure goes through the
//! audited seam and its form renders from the request schema (the next step
//! of #1261). Not a subscription: a call is one answer, once.
//!
//! **Stale answers.** The old arms dropped a reply into whatever device was
//! selected when it arrived, and a slow answer to an old sort overwrote the
//! new one. A reply carries the device and the params it answers, and
//! [`Calls::apply`] keeps it only when both match the call in flight.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use zensight_common::decode_with_encoding;

pub use zenkey_fleet::report::PageSignal;

use crate::view::specialized::fetch::Fetch;

/// How long a call waits for the last reply. Longer than a poll interval,
/// shorter than the operator's patience: a procedure that has not answered
/// in this long is reported as not answering, not left spinning.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One answered read procedure: the reply as the producer sent it, and what
/// the envelope said about it when the reply was one.
pub struct Reply {
    pub value: serde_json::Value,
    /// The RFC 05 §3.2 page signal, when the reply was an envelope. `None`
    /// is "not an envelope", which for a bare list means "the whole answer"
    /// only by the producer's convention, not the wire's.
    pub page: Option<PageSignal>,
    /// When the answer landed, epoch millis.
    pub received_ms: i64,
    /// The typed projection a bespoke view asked for, decoded once: a view
    /// function borrows `&state` and hands Iced elements that borrow the
    /// rows, so the rows must live in the state, not on the view's stack.
    typed: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reply")
            .field("value", &self.value)
            .field("page", &self.page)
            .field("received_ms", &self.received_ms)
            .finish_non_exhaustive()
    }
}

impl Clone for Reply {
    /// The wire value clones; the typed projection is decoded again on
    /// first use — it is a cache, not state.
    fn clone(&self) -> Self {
        Reply {
            value: self.value.clone(),
            page: self.page.clone(),
            received_ms: self.received_ms,
            typed: std::sync::OnceLock::new(),
        }
    }
}

impl PartialEq for Reply {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
            && self.page == other.page
            && self.received_ms == other.received_ms
    }
}

impl Reply {
    /// A reply from a value, as a call folds one. Reads the page envelope.
    pub fn new(value: serde_json::Value, received_ms: i64) -> Self {
        let page = page_signal_of(&value);
        Reply {
            value,
            page,
            received_ms,
            typed: std::sync::OnceLock::new(),
        }
    }

    /// The reply as a type, decoded once and borrowed from then on — the
    /// form a view uses, so a table can borrow its rows for as long as the
    /// element lives. One reply, one type: a second type asked of the same
    /// reply is an error, not a second decode, because two views reading
    /// one procedure as two types is the bug this makes visible.
    pub fn decoded<T>(&self) -> Result<&T, String>
    where
        T: DeserializeOwned + std::any::Any + Send + Sync,
    {
        self.decoded_with(|_: &mut T| {})
    }

    /// [`Reply::decoded`] with a normalisation applied once, at decode: the
    /// order a view wants its rows in before the table's own sort (newest
    /// first, worst peer first), which the old fetch arms applied on arrival.
    pub fn decoded_with<T>(&self, normalise: impl FnOnce(&mut T)) -> Result<&T, String>
    where
        T: DeserializeOwned + std::any::Any + Send + Sync,
    {
        let slot = self.typed.get_or_init(|| {
            let mut decoded = self.decode::<T>();
            if let Ok(value) = &mut decoded {
                normalise(value);
            }
            Box::new(decoded) as Box<dyn std::any::Any + Send + Sync>
        });
        match slot.downcast_ref::<Result<T, String>>() {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(e.clone()),
            None => Err(format!(
                "reply already decoded as another type than {}",
                std::any::type_name::<T>()
            )),
        }
    }

    /// Decode the reply as a type — for a bespoke renderer that draws it as
    /// something the schema alone could not (a process table with a unit
    /// chip per row). An envelope decodes as its `items` when `T` is the
    /// item list, so a producer moving to `Page<T>` does not break the view.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, String> {
        let attempt = serde_json::from_value::<T>(self.value.clone());
        match (attempt, self.page.as_ref(), self.value.get("items")) {
            (Ok(t), _, _) => Ok(t),
            (Err(_), Some(_), Some(items)) => {
                serde_json::from_value(items.clone()).map_err(|e| e.to_string())
            }
            (Err(e), _, _) => Err(e.to_string()),
        }
    }

    /// The rows of the answer: an envelope's `items`, a bare list's
    /// elements, else the value alone.
    pub fn items(&self) -> &[serde_json::Value] {
        match (&self.page, &self.value) {
            (Some(_), serde_json::Value::Object(o)) => {
                o.get("items").and_then(|i| i.as_array()).map_or(&[], |v| v)
            }
            (_, serde_json::Value::Array(a)) => a,
            _ => std::slice::from_ref(&self.value),
        }
    }
}

/// The page signal of a reply value, read exactly as `zenkey_fleet` reads
/// it (`CallAnswer::page_signal`): an object with a *boolean* `partial` is
/// an envelope; anything else is not, and a `truncated` field is not seen.
pub fn page_signal_of(value: &serde_json::Value) -> Option<PageSignal> {
    let o = value.as_object()?;
    let partial = o.get("partial")?.as_bool()?;
    Some(PageSignal {
        partial,
        next_cursor: o
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .map(str::to_string),
        scanned: o.get("scanned").and_then(|n| n.as_u64()),
        covers_from: o
            .get("covers_from")
            .and_then(|c| c.as_str())
            .map(str::to_string),
    })
}

/// Where a procedure's call stands, with the answer borrowed as a type —
/// what a view reads (#1261): the four states of [`Fetch`], the answer
/// decoded once through [`Reply::decoded_with`].
#[derive(Debug)]
pub enum Answer<'a, T> {
    Idle,
    Loading,
    Ready(&'a T),
    /// The call failed, or the answer was not the type the view expected.
    Error(String),
}

impl<'a, T> Answer<'a, T> {
    pub fn is_loading(&self) -> bool {
        matches!(self, Answer::Loading)
    }

    pub fn ready(&self) -> Option<&'a T> {
        match self {
            Answer::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Answer::Error(message) => Some(message.as_str()),
            _ => None,
        }
    }
}

/// One procedure's call state on a device: the params of the last call and
/// where it stands.
#[derive(Debug, Clone, Default)]
pub struct CallState {
    /// The `?`-less query string of the call in flight or answered
    /// (`sort=cpu&top=50`); empty for a call without parameters.
    pub params: String,
    pub fetch: Fetch<Reply>,
}

/// The calls a device view has made, by procedure path.
#[derive(Debug, Clone, Default)]
pub struct Calls {
    calls: BTreeMap<String, CallState>,
}

static IDLE: Fetch<Reply> = Fetch::Idle;

impl Calls {
    pub fn get(&self, procedure: &str) -> Option<&CallState> {
        self.calls.get(procedure)
    }

    /// Where a procedure's call stands; `Idle` when never called.
    pub fn fetch(&self, procedure: &str) -> &Fetch<Reply> {
        self.calls.get(procedure).map_or(&IDLE, |c| &c.fetch)
    }

    /// The params of the last call to a procedure, `""` when never called.
    pub fn params(&self, procedure: &str) -> &str {
        self.calls.get(procedure).map_or("", |c| c.params.as_str())
    }

    /// The answered reply, when there is one.
    pub fn reply(&self, procedure: &str) -> Option<&Reply> {
        self.fetch(procedure).ready()
    }

    /// The answered reply as a type, borrowed — see [`Reply::decoded`].
    /// `None` when there is no answer; `Some(Err)` when the answer is not
    /// the type the view expected, which the view shows as a failure and
    /// never as an empty table.
    pub fn decoded<T>(&self, procedure: &str) -> Option<Result<&T, String>>
    where
        T: DeserializeOwned + std::any::Any + Send + Sync,
    {
        self.reply(procedure).map(Reply::decoded::<T>)
    }

    pub fn is_loading(&self, procedure: &str) -> bool {
        self.fetch(procedure).is_loading()
    }

    /// Where a procedure stands, its answer as a type — see [`Answer`].
    pub fn answer<T>(&self, procedure: &str) -> Answer<'_, T>
    where
        T: DeserializeOwned + std::any::Any + Send + Sync,
    {
        self.answer_with(procedure, |_: &mut T| {})
    }

    /// [`Calls::answer`] with a normalisation applied once at decode — see
    /// [`Reply::decoded_with`].
    pub fn answer_with<T>(&self, procedure: &str, normalise: impl FnOnce(&mut T)) -> Answer<'_, T>
    where
        T: DeserializeOwned + std::any::Any + Send + Sync,
    {
        match self.fetch(procedure) {
            Fetch::Idle => Answer::Idle,
            Fetch::Loading => Answer::Loading,
            Fetch::Error(e) => Answer::Error(e.clone()),
            Fetch::Ready(reply) => match reply.decoded_with(normalise) {
                Ok(value) => Answer::Ready(value),
                Err(e) => Answer::Error(e),
            },
        }
    }

    /// Mark a call in flight. Replaces whatever the procedure held, so the
    /// view shows "fetching" and not the old answer under a new sort.
    pub fn loading(&mut self, procedure: &str, params: &str) {
        self.calls.insert(
            procedure.to_string(),
            CallState {
                params: params.to_string(),
                fetch: Fetch::Loading,
            },
        );
    }

    /// Land an answer. Kept only when the procedure has a call in flight
    /// with the same params: a reply to a superseded call is dropped, and so
    /// is one for a call the view never made.
    pub fn apply(&mut self, procedure: &str, params: &str, result: Result<Reply, String>) -> bool {
        match self.calls.get_mut(procedure) {
            Some(call) if call.fetch.is_loading() && call.params == params => {
                call.fetch = Fetch::from_result(result);
                true
            }
            _ => false,
        }
    }

    /// Put an answered reply in place without a call — the test seam, and
    /// the demo's.
    pub fn set_ready(&mut self, procedure: &str, params: &str, value: serde_json::Value) {
        self.calls.insert(
            procedure.to_string(),
            CallState {
                params: params.to_string(),
                fetch: Fetch::Ready(Reply::new(value, 0)),
            },
        );
    }

    /// Put a failed answer in place without a call — the test seam.
    pub fn set_failed(&mut self, procedure: &str, error: &str) {
        self.calls.insert(
            procedure.to_string(),
            CallState {
                params: String::new(),
                fetch: Fetch::Error(error.to_string()),
            },
        );
    }

    /// Forget a procedure's answer, so its panel offers the call again.
    pub fn clear(&mut self, procedure: &str) {
        self.calls.remove(procedure);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &CallState)> {
        self.calls.iter()
    }
}

/// The selector a call GETs. `Some(origin)` is the drilled-in host's
/// concrete key (the device's origin, once the source→origin map has learned
/// it); `None` is the fleet selector, whose answers are folded together.
pub fn procedure_key(
    origin: Option<&zenkey::RemoteOrigin>,
    producer: &str,
    procedure: &str,
    params: &str,
) -> String {
    let key = match origin {
        Some(o) => zensight_common::origin_rpc_key(o, producer, procedure),
        None => zensight_common::fleet_rpc_key(producer, procedure),
    };
    if params.is_empty() {
        key
    } else {
        format!("{key}?{params}")
    }
}

/// Fold the decoded replies of one call into the answer.
///
/// One origin-scoped key names one producer instance (RFC 05 §2.1), but
/// nothing on the wire enforces that, and a host running the live sensor
/// beside an idle twin answers twice — so a *scoped* call keeps the fullest
/// list (the twin's empty ring never wins, #980), or the first non-list. A
/// *fleet* call is a fan-in: lists are concatenated, a non-list answer is
/// the first.
pub fn fold_replies(scoped: bool, replies: Vec<serde_json::Value>) -> Option<serde_json::Value> {
    if replies.is_empty() {
        return None;
    }
    let all_lists = replies.iter().all(|v| v.is_array());
    if !all_lists {
        return replies.into_iter().next();
    }
    if scoped {
        replies
            .into_iter()
            .max_by_key(|v| v.as_array().map_or(0, Vec::len))
    } else {
        let mut out = Vec::new();
        for v in replies {
            if let serde_json::Value::Array(mut a) = v {
                out.append(&mut a);
            }
        }
        Some(serde_json::Value::Array(out))
    }
}

/// GET a read procedure and fold its replies. Iced-independent.
///
/// `Err` carries what the view says: a producer's own error reply verbatim
/// when there was one, else "no `<producer>` sensor answered".
pub async fn call(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
    producer: String,
    procedure: String,
    params: String,
) -> Result<Reply, String> {
    let key = procedure_key(origin.as_ref(), &producer, &procedure, &params);
    let replies = session
        .get(&key)
        .target(zenoh::query::QueryTarget::All)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .timeout(TIMEOUT)
        .await
        .map_err(|e| format!("call {key}: {e}"))?;
    let mut values = Vec::new();
    let mut refused: Option<String> = None;
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => match decode_with_encoding::<serde_json::Value>(
                sample.encoding(),
                &sample.payload().to_bytes(),
            ) {
                Ok(v) => values.push(v),
                Err(e) => tracing::warn!(key = %key, error = %e, "call: reply decode failed"),
            },
            Err(e) => {
                let text = e
                    .payload()
                    .try_to_string()
                    .map_or_else(|_| "error reply".to_string(), |s| s.into_owned());
                refused.get_or_insert(text);
            }
        }
    }
    match fold_replies(origin.is_some(), values) {
        Some(value) => Ok(Reply::new(value, now_ms())),
        None => Err(refused.unwrap_or_else(|| format!("No {producer} sensor responded"))),
    }
}

/// Epoch millis now — when an answer landed.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_origin() -> zenkey::RemoteOrigin {
        zenkey::RemoteOrigin::parse("h-3fa9c2d41b7e").expect("valid test origin")
    }

    #[test]
    fn key_is_host_scoped_or_fleet_with_params() {
        assert_eq!(
            procedure_key(
                Some(&test_origin()),
                "sysinfo",
                "processes",
                "sort=cpu&top=50"
            ),
            "v1/h-3fa9c2d41b7e/@rpc/sysinfo/processes?sort=cpu&top=50"
        );
        assert_eq!(
            procedure_key(None, "sysinfo", "processes", "sort=mem&top=50"),
            "v1/*/@rpc/sysinfo/processes?sort=mem&top=50"
        );
        assert_eq!(
            procedure_key(None, "netflow", "flows", "max=200"),
            "v1/*/@rpc/netflow/flows?max=200"
        );
        assert_eq!(
            procedure_key(Some(&test_origin()), "sysinfo", "latency", ""),
            "v1/h-3fa9c2d41b7e/@rpc/sysinfo/latency"
        );
    }

    #[test]
    fn envelope_is_recognised_by_a_boolean_partial_only() {
        let page = json!({ "items": [1, 2], "partial": true, "next_cursor": "k9", "scanned": 40 });
        let signal = page_signal_of(&page).expect("an envelope");
        assert!(signal.partial);
        assert_eq!(signal.next_cursor.as_deref(), Some("k9"));
        assert_eq!(signal.scanned, Some(40));
        // `truncated` is the historian's old spelling: not an envelope.
        assert!(page_signal_of(&json!({ "items": [], "truncated": true })).is_none());
        // A string `partial` is not a marker either.
        assert!(page_signal_of(&json!({ "items": [], "partial": "true" })).is_none());
        assert!(page_signal_of(&json!([1, 2])).is_none());
    }

    #[test]
    fn items_are_the_envelope_rows_or_the_list_or_the_value() {
        let page = Reply::new(json!({ "items": [{ "a": 1 }], "partial": false }), 0);
        assert_eq!(page.items().len(), 1);
        let list = Reply::new(json!([1, 2, 3]), 0);
        assert_eq!(list.items().len(), 3);
        let one = Reply::new(json!({ "available": false }), 0);
        assert_eq!(one.items().len(), 1);
        assert_eq!(one.items()[0]["available"], json!(false));
    }

    #[test]
    fn decode_reads_a_list_from_a_bare_reply_or_an_envelope() {
        let bare = Reply::new(json!([{ "n": 1 }, { "n": 2 }]), 0);
        let rows: Vec<serde_json::Value> = bare.decode().expect("a list");
        assert_eq!(rows.len(), 2);
        // The same view keeps working when the producer moves to `Page<T>`.
        let page = Reply::new(json!({ "items": [{ "n": 1 }], "partial": true }), 0);
        let rows: Vec<serde_json::Value> = page.decode().expect("the items");
        assert_eq!(rows.len(), 1);
        // The wrong shape is an error the view shows, never an empty table.
        let wrong = Reply::new(json!({ "available": false }), 0);
        assert!(wrong.decode::<Vec<u8>>().is_err());
    }

    #[test]
    fn answer_normalises_once_at_decode() {
        let mut calls = Calls::default();
        assert!(matches!(calls.answer::<Vec<u64>>("events"), Answer::Idle));
        calls.set_ready("events", "", json!([1, 3, 2]));
        let newest_first = |v: &mut Vec<u64>| v.sort_by_key(|n| std::cmp::Reverse(*n));
        let rows = calls
            .answer_with("events", newest_first)
            .ready()
            .expect("decoded");
        assert_eq!(rows, &vec![3, 2, 1]);
        // The second read is the same decode: normalised once, not per render.
        let again = calls.answer::<Vec<u64>>("events").ready().unwrap();
        assert!(std::ptr::eq(rows, again));
        calls.set_failed("events", "no sensor");
        assert_eq!(
            calls.answer::<Vec<u64>>("events").error(),
            Some("no sensor")
        );
        calls.set_ready("events", "", json!({ "not": "a list" }));
        assert!(calls.answer::<Vec<u64>>("events").error().is_some());
    }

    #[test]
    fn decoded_is_borrowed_once_and_typed_once() {
        let reply = Reply::new(json!([{ "n": 1 }]), 0);
        let a: &Vec<serde_json::Value> = reply.decoded().expect("a list");
        let b: &Vec<serde_json::Value> = reply.decoded().expect("the same list");
        assert!(std::ptr::eq(a, b), "decoded once, borrowed twice");
        // A second type on the same reply is refused, not re-decoded.
        assert!(reply.decoded::<Vec<u64>>().is_err());
        // A clone starts over: it is a cache, not state.
        let again = reply.clone();
        assert!(again.decoded::<Vec<u64>>().is_err());
        assert!(again.decoded::<Vec<serde_json::Value>>().is_err());
    }

    #[test]
    fn a_scoped_call_keeps_the_fullest_list_and_a_fleet_call_concatenates() {
        let live = json!([{ "pid": 1 }, { "pid": 2 }]);
        let idle_twin = json!([]);
        let folded = fold_replies(true, vec![idle_twin.clone(), live.clone()]).unwrap();
        assert_eq!(
            folded.as_array().unwrap().len(),
            2,
            "the idle twin never wins"
        );
        let folded = fold_replies(false, vec![live, json!([{ "pid": 9 }])]).unwrap();
        assert_eq!(folded.as_array().unwrap().len(), 3, "fleet fan-in appends");
        // A non-list answer is the first, whichever selector asked.
        let first = fold_replies(true, vec![json!({ "a": 1 }), json!({ "a": 2 })]).unwrap();
        assert_eq!(first["a"], json!(1));
        assert!(fold_replies(true, Vec::new()).is_none());
    }

    #[test]
    fn apply_keeps_only_the_answer_to_the_call_in_flight() {
        let mut calls = Calls::default();
        assert!(matches!(calls.fetch("processes"), Fetch::Idle));
        calls.loading("processes", "sort=cpu&top=50");
        assert!(calls.is_loading("processes"));
        assert_eq!(calls.params("processes"), "sort=cpu&top=50");
        // The user re-sorted before the first answer landed.
        calls.loading("processes", "sort=mem&top=50");
        let stale = Reply::new(json!([{ "pid": 1 }]), 0);
        assert!(
            !calls.apply("processes", "sort=cpu&top=50", Ok(stale)),
            "an answer to the superseded sort is dropped"
        );
        assert!(calls.is_loading("processes"));
        let fresh = Reply::new(json!([{ "pid": 2 }]), 0);
        assert!(calls.apply("processes", "sort=mem&top=50", Ok(fresh)));
        assert_eq!(calls.reply("processes").unwrap().items().len(), 1);
        // A reply for a call the view never made is dropped too.
        assert!(!calls.apply("latency", "", Ok(Reply::new(json!({}), 0))));
        // A failure lands like an answer.
        calls.loading("latency", "");
        assert!(calls.apply("latency", "", Err("no sysinfo sensor".into())));
        assert_eq!(calls.fetch("latency").error(), Some("no sysinfo sensor"));
        calls.clear("latency");
        assert!(matches!(calls.fetch("latency"), Fetch::Idle));
    }
}
