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
//! **Writes** are the other half (#1261, design §5.5): a write procedure is a
//! GET *with a body* on `@rpc/<producer>/<procedure>`, answered through the
//! producer's audited seam (`served::serve_write_queryable`, #957) — a value
//! reply is the outcome, an `error/gated` reply error is the refusal, and it
//! names the switch that refused. The GUI arms one ([`Armed`]: the request,
//! how it is confirmed, how long to wait), confirms it, and lands the outcome
//! in [`Writes`]; [`write`] is the transport. A write is addressed to one
//! origin and nothing else: there is no fleet spelling of it here, because
//! "restart nginx" on every host serving the sensor is not a slip a GUI may
//! make. Not a subscription: a call is one answer, once.
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

/// Which surface a call is asked from and lands on (#1261): the selected
/// device's view, or one of the two hand-written surfaces (design §5.6)
/// that join a producer's call into rows of their own — the Security
/// drill-down and the topology edge panel, whose flow↔process join asks
/// `netlink/sockets` for a flow it got from netring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallSurface {
    #[default]
    Device,
    Security,
    Topology,
}

/// One read-procedure call as a view asks it (#1261): what
/// [`Message::Call`](crate::message::Message::Call) carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub surface: CallSurface,
    /// The producer asked. `None` is the selected device's own, on its
    /// origin; a producer named here is asked fleet-wide, because the host
    /// that can answer is not the one whose view is open — the socket a
    /// netring flow belongs to lives on the endpoint's host.
    pub producer: Option<String>,
    pub procedure: String,
    /// The `?`-less query string (`sort=cpu&top=50`); empty for none.
    pub params: String,
    /// What the answer is filed under in [`Calls`]. `None` is the
    /// procedure: one answer per procedure at a time, which is what every
    /// panel wants. A caller that needs two answers to one procedure with
    /// different params — the join's two endpoints — keys them itself.
    pub key: Option<String>,
    /// The host asked, when the surface chose one (the Security pane's
    /// netring host). `None` is the selected device's origin for its own
    /// producer, the fleet for another's.
    pub origin: Option<zenkey::RemoteOrigin>,
}

impl Request {
    /// The selected device's own procedure, keyed by its name.
    pub fn new(procedure: impl Into<String>, params: impl Into<String>) -> Self {
        Request {
            surface: CallSurface::Device,
            producer: None,
            procedure: procedure.into(),
            params: params.into(),
            key: None,
            origin: None,
        }
    }

    pub fn on(mut self, surface: CallSurface) -> Self {
        self.surface = surface;
        self
    }

    pub fn of(mut self, producer: impl Into<String>) -> Self {
        self.producer = Some(producer.into());
        self
    }

    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn at(mut self, origin: zenkey::RemoteOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// What the answer is filed under.
    pub fn key(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.procedure)
    }
}

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

/// One call's state: the procedure and the params of the last call under
/// its key, and where it stands.
#[derive(Debug, Clone, Default)]
pub struct CallState {
    /// The procedure asked — the key itself, unless the caller keyed it.
    pub procedure: String,
    /// The `?`-less query string of the call in flight or answered
    /// (`sort=cpu&top=50`); empty for a call without parameters.
    pub params: String,
    pub fetch: Fetch<Reply>,
}

/// The calls a surface has made, by key — the procedure path unless the
/// caller chose one (see [`Request::key`]).
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

    /// Mark a call in flight under its procedure. Replaces whatever the
    /// procedure held, so the view shows "fetching" and not the old answer
    /// under a new sort.
    pub fn loading(&mut self, procedure: &str, params: &str) {
        self.loading_as(procedure, procedure, params);
    }

    /// Mark a call in flight under a caller-chosen key.
    pub fn loading_as(&mut self, key: &str, procedure: &str, params: &str) {
        self.calls.insert(
            key.to_string(),
            CallState {
                procedure: procedure.to_string(),
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
                procedure: procedure.to_string(),
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
                procedure: procedure.to_string(),
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

/// How an armed write is confirmed before it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// A second click: the row swaps to confirm/cancel.
    Click,
    /// Typing `expected` — for an action that cuts power: a `[confirm]`
    /// button one slip away from a live one is not a confirmation.
    Typed { expected: String },
}

/// A write procedure the operator has armed and not yet confirmed (#1261):
/// what will be sent, to whom, how it reads on screen, and how it is
/// confirmed. One write is armed at a time, app-wide: arming on one
/// surface disarms the other, so `Confirm` is never ambiguous.
#[derive(Debug, Clone, PartialEq)]
pub struct Armed {
    /// The surface that armed it and shows its confirmation.
    pub surface: CallSurface,
    /// The producer written to; `None` is the selected device's own.
    pub producer: Option<String>,
    /// The host written to; `None` is the surface's own — the selected
    /// device's origin, the Security pane's chosen host. Never the fleet:
    /// a write with no host to address is refused, not broadcast.
    pub origin: Option<zenkey::RemoteOrigin>,
    pub procedure: String,
    /// The request body as JSON — the request type the registry declares.
    pub request: serde_json::Value,
    /// How the action reads in a sentence (`restart nginx.service`,
    /// `cycle outlet pdu-a/3`).
    pub label: String,
    pub confirmation: Confirmation,
    /// What the operator has typed so far, for a [`Confirmation::Typed`].
    pub typed: String,
    /// How long to wait for the outcome: past a producer's own job wait
    /// (systemd blocks until the job resolves), or an action reads as a
    /// failure while it is still succeeding.
    pub timeout: std::time::Duration,
}

impl Armed {
    /// Whether the confirmation holds. Typed: exact, trimmed only of
    /// surrounding whitespace — the point of typing the name is that it
    /// cannot be produced by a slip.
    pub fn confirmed(&self) -> bool {
        match &self.confirmation {
            Confirmation::Click => true,
            Confirmation::Typed { expected } => self.typed.trim() == expected,
        }
    }

    /// A string field of the request, for a view matching a row to the
    /// write that is armed or in flight on it.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.request.get(name).and_then(|v| v.as_str())
    }
}

/// Why a write produced no outcome value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFailure {
    /// The producer refused: the `error/gated` reply error it sent back,
    /// with the switch that refused when it named one (#866, #957).
    Refused {
        error: String,
        message: String,
        refused_by: Option<String>,
    },
    /// Replies closed well before the deadline — nobody serves the key, so
    /// the action is off or the producer is offline.
    NotServed,
    /// The deadline elapsed. The request may have been accepted and may
    /// still be running; this is emphatically not a failure, and must not be
    /// reported as one.
    StillRunning {
        waited_secs: u64,
    },
    Transport(String),
}

impl WriteFailure {
    /// The sentence a toast shows.
    pub fn sentence(&self) -> String {
        match self {
            WriteFailure::Refused {
                error,
                message,
                refused_by,
            } => match refused_by {
                Some(switch) => format!("Refused — {error}: {message} (refused by {switch})"),
                None => format!("Refused — {error}: {message}"),
            },
            WriteFailure::NotServed => {
                "nothing serves this procedure on this host — the action is disabled or the sensor is offline"
                    .to_string()
            }
            WriteFailure::StillRunning { waited_secs } => {
                format!("No reply within {waited_secs}s — the job may still be running")
            }
            WriteFailure::Transport(e) => format!("failed: {e}"),
        }
    }

    /// A deadline that elapsed is a warning, everything else an error.
    pub fn is_warning(&self) -> bool {
        matches!(self, WriteFailure::StillRunning { .. })
    }
}

/// The write machine of a device view (#1261): one armed write, one in
/// flight, and the last outcome per procedure.
#[derive(Debug, Clone, Default)]
pub struct Writes {
    pub armed: Option<Armed>,
    /// Issued and not yet answered. A producer's write blocks until the job
    /// resolves, so without this a second click would queue a second job.
    pub inflight: Option<Armed>,
    /// The last outcome per procedure: the reply, or the failure's sentence.
    pub last: BTreeMap<String, Result<Reply, String>>,
}

impl Writes {
    pub fn arm(&mut self, armed: Armed) {
        self.armed = Some(armed);
    }

    pub fn disarm(&mut self) {
        self.armed = None;
    }

    pub fn typed(&mut self, typed: String) {
        if let Some(armed) = &mut self.armed {
            armed.typed = typed;
        }
    }

    /// Take the armed write if its confirmation holds — the check the app
    /// repeats, so a message arriving any other way cannot skip it.
    pub fn confirm(&mut self) -> Option<Armed> {
        if self.armed.as_ref().is_some_and(Armed::confirmed) {
            self.armed.take()
        } else {
            None
        }
    }

    /// The armed write to `procedure`, if that is what is armed.
    pub fn armed_for(&self, procedure: &str) -> Option<&Armed> {
        self.armed.as_ref().filter(|a| a.procedure == procedure)
    }

    /// The write to `procedure` in flight, if one is.
    pub fn inflight_for(&self, procedure: &str) -> Option<&Armed> {
        self.inflight.as_ref().filter(|a| a.procedure == procedure)
    }

    /// The last outcome of `procedure` as a type, when it answered.
    pub fn last_as<T: DeserializeOwned>(&self, procedure: &str) -> Option<T> {
        self.last
            .get(procedure)?
            .as_ref()
            .ok()
            .and_then(|reply| reply.decode::<T>().ok())
    }
}

/// GET a write procedure with a body on one origin, and read the outcome
/// (#1261). Iced-independent.
///
/// A concrete single-origin key has exactly one queryable, so BestMatching
/// is the honest target here; QueryTarget::All is for fleet fan-in (RFC 05
/// §2.1). Zenoh reports "nobody served the key" and "the deadline elapsed"
/// identically, as a closed reply channel; elapsed time against the deadline
/// is the only way to tell them apart — a heuristic, but the two need very
/// different wording: one is an error, the other a job that may well be
/// succeeding.
pub async fn write(
    session: Arc<zenoh::Session>,
    origin: zenkey::RemoteOrigin,
    producer: String,
    procedure: String,
    request: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<Reply, WriteFailure> {
    let key = zensight_common::origin_rpc_key(&origin, &producer, &procedure);
    let payload = serde_json::to_vec(&request)
        .map_err(|e| WriteFailure::Transport(format!("Failed to encode request: {e}")))?;
    let started = std::time::Instant::now();
    let replies = session
        .get(&key)
        .payload(payload)
        .target(zenoh::query::QueryTarget::BestMatching)
        .timeout(timeout)
        .await
        .map_err(|e| WriteFailure::Transport(e.to_string()))?;
    match replies.recv_async().await {
        Ok(reply) => match reply.result() {
            Ok(sample) => decode_with_encoding::<serde_json::Value>(
                sample.encoding(),
                &sample.payload().to_bytes(),
            )
            .map(|value| Reply::new(value, now_ms()))
            .map_err(|e| WriteFailure::Transport(format!("Undecodable reply: {e}"))),
            Err(err) => {
                let parsed: Option<zensight_common::rpc::RpcError> =
                    serde_json::from_slice(&err.payload().to_bytes()).ok();
                Err(match parsed {
                    Some(e) => WriteFailure::Refused {
                        error: e.error,
                        message: e.message,
                        refused_by: e.refused_by,
                    },
                    None => WriteFailure::Refused {
                        error: "error".to_string(),
                        message: "refused".to_string(),
                        refused_by: None,
                    },
                })
            }
        },
        Err(_) => {
            let waited = started.elapsed();
            if waited + std::time::Duration::from_millis(250) >= timeout {
                Err(WriteFailure::StillRunning {
                    waited_secs: waited.as_secs(),
                })
            } else {
                Err(WriteFailure::NotServed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn armed(confirmation: Confirmation) -> Armed {
        Armed {
            surface: CallSurface::Device,
            producer: None,
            origin: None,
            procedure: "action/set".into(),
            request: json!({ "device": "pdu-a", "outlet": "3", "verb": "cycle" }),
            label: "cycle outlet pdu-a/3".into(),
            confirmation,
            typed: String::new(),
            timeout: std::time::Duration::from_secs(30),
        }
    }

    /// **Typing the name is the confirmation.** A `[confirm]` button one slip
    /// away from a live one is not a confirmation, and this cuts power.
    #[test]
    fn a_typed_confirmation_must_match_exactly() {
        let mut a = armed(Confirmation::Typed {
            expected: "3".into(),
        });
        for typed in ["", "4", "33", "outlet 3", "  "] {
            a.typed = typed.to_string();
            assert!(!a.confirmed(), "{typed:?} must not arm the button");
        }
        a.typed = "3".into();
        assert!(a.confirmed());
        // Surrounding whitespace is forgiven; nothing else is.
        a.typed = " 3 ".into();
        assert!(a.confirmed());
        assert!(armed(Confirmation::Click).confirmed());
        assert_eq!(a.field("outlet"), Some("3"));
        assert_eq!(a.field("missing"), None);
    }

    #[test]
    fn the_write_machine_confirms_only_what_holds() {
        let mut w = Writes::default();
        assert!(w.confirm().is_none(), "nothing armed, nothing to confirm");
        w.arm(armed(Confirmation::Typed {
            expected: "3".into(),
        }));
        assert!(w.armed_for("action/set").is_some());
        assert!(w.armed_for("other").is_none());
        assert!(w.confirm().is_none(), "not typed yet");
        assert!(w.armed.is_some(), "a failed confirm keeps the arming");
        w.typed("3".into());
        let taken = w.confirm().expect("confirmed");
        assert!(w.armed.is_none());
        w.inflight = Some(taken);
        assert!(w.inflight_for("action/set").is_some());
        w.disarm();
        w.last.insert(
            "action/set".into(),
            Ok(Reply::new(json!({ "accepted": true, "outlet": "3" }), 0)),
        );
        let last: serde_json::Value = w.last_as("action/set").expect("decoded");
        assert_eq!(last["outlet"], json!("3"));
        w.last.insert("action/set".into(), Err("boom".into()));
        assert!(w.last_as::<serde_json::Value>("action/set").is_none());
    }

    #[test]
    fn a_failure_has_a_sentence_and_a_severity() {
        let refused = WriteFailure::Refused {
            error: "error/gated".into(),
            message: "actions are off".into(),
            refused_by: Some("actions.enabled".into()),
        };
        assert_eq!(
            refused.sentence(),
            "Refused — error/gated: actions are off (refused by actions.enabled)"
        );
        assert!(!refused.is_warning());
        let late = WriteFailure::StillRunning { waited_secs: 35 };
        assert!(late.is_warning());
        assert!(late.sentence().contains("35s"));
    }

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

    /// Two answers to one procedure coexist under caller-chosen keys
    /// (#1261): the join's two endpoints, each with its own params, land
    /// beside each other instead of the second replacing the first.
    #[test]
    fn a_keyed_call_coexists_with_another_to_the_same_procedure() {
        let mut calls = Calls::default();
        calls.loading_as("attribution:a → b#a", "sockets", "ip=10.0.0.5");
        calls.loading_as("attribution:a → b#b", "sockets", "ip=1.1.1.1");
        assert!(calls.apply(
            "attribution:a → b#a",
            "ip=10.0.0.5",
            Ok(Reply::new(json!([]), 0))
        ));
        assert!(calls.is_loading("attribution:a → b#b"));
        assert!(calls.reply("attribution:a → b#a").is_some());
        assert_eq!(
            calls.get("attribution:a → b#a").unwrap().procedure,
            "sockets"
        );
        // The default key is the procedure, as before.
        calls.loading("flows", "top=50");
        assert_eq!(calls.get("flows").unwrap().procedure, "flows");
        let request = Request::new("sockets", "ip=1.1.1.1").keyed("attribution:a → b#b");
        assert_eq!(request.key(), "attribution:a → b#b");
        assert_eq!(Request::new("flows", "").key(), "flows");
    }
}
