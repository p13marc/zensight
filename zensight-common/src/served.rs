//! Serve-time registry conformance for `@rpc` (RFC 08 §5/§6.1, issue #484).
//!
//! [`crate::metric_guard`] checks one direction — *every published key is
//! buildable from a registry entry*. This checks the other, which RFC 08 §6.1
//! upgraded to a **MUST**: *every registered procedure is actually served by
//! the build that advertises it*.
//!
//! The directions are not mirror images, and the first does not imply the
//! second. A registry may be a strict superset of what the code does and every
//! published key still builds — and that superset is exactly what `introspect`
//! ships to the fleet as truth. The #453 audit found **seven** such surfaces
//! advertised by builds that served none of them; review had not caught them,
//! because review cannot: only a check can.
//!
//! So: every queryable declaration goes through [`serve_queryable`], which
//! records the served key, and [`check_registry_coverage`] compares that set
//! against the compiled registry slice when the producer serves `introspect`.
//! Debug builds panic — a sensor's own tests fail on a registry that lies.
//! Release builds warn, loudly and once, because a running fleet is better
//! served by a noisy sensor than a dead one.
//!
//! # What this module does NOT check
//!
//! **Subjects.** RFC 08 §6.1's MUST says "every subject *and* procedure", and
//! this is the procedure half only. That is a structural limit, not a backlog
//! item: a procedure is served by a declaration the process makes once and
//! unconditionally at startup — an observable event — while a publisher is
//! declared lazily on the first put ([`crate::PublisherRegistry`]), so at
//! `introspect` time a healthy producer has declared almost nothing. Later the
//! served set is *still* legitimately incomplete, because it is the
//! intersection of "this build can emit it" with "this host has that hardware
//! and permission". A runtime check here cannot separate a lying registry from
//! a boring host.
//!
//! The subject half is therefore checked at test time by
//! [`crate::registry_audit`], against the producer's mappers rather than
//! against this host. See `zensight-common/docs/registry-honesty.md` for the
//! full picture — which of the four checks covers what, and which producers
//! are still uncovered (#648).
//!
//! **Conditional surfaces.** A registry entry cannot say "only in builds with
//! feature X" — the TOML schema is owned by the external `zenkey` crate. Until
//! it can, a conditional procedure must be declared anyway and answer
//! [`serve_unavailable`], never left undeclared.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn served() -> &'static Mutex<HashSet<String>> {
    static SERVED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SERVED.get_or_init(Default::default)
}

/// Woken whenever a key joins the served set, so [`await_registry_coverage`]
/// can wait for the set to close instead of sampling it once.
fn served_changed() -> &'static tokio::sync::Notify {
    static CHANGED: OnceLock<tokio::sync::Notify> = OnceLock::new();
    CHANGED.get_or_init(Default::default)
}

/// Declare a queryable and record it as served (#484).
///
/// A thin wrapper over [`zenoh::Session::declare_queryable`] so the recording
/// cannot drift from the declaration: this is the only place the process
/// learns what it actually serves. Recording where keys are *built* would be
/// cheaper and wrong — building a key is not serving it, and a coverage check
/// that trusts a `format!` is its own small lie.
pub async fn serve_queryable(
    session: &zenoh::Session,
    key: &str,
) -> zenoh::Result<zenoh::query::Queryable<zenoh::handlers::FifoChannelHandler<zenoh::query::Query>>>
{
    // #782: a state-class selector is a *seed*, and its replies must be
    // stamped. This seam cannot stamp them — it hands out a bare `Query` whose
    // `reply()` carries no timestamp — so route those through
    // `serve_state_queryable`, which can and which has no unstamped reply path
    // to reach for.
    //
    // Debug-panics and release-warns, the same policy as
    // `check_registry_coverage` below and for the same reason: a sensor's own
    // tests should fail on it, and a running fleet is better served by a noisy
    // producer than a dead one.
    if crate::keyexpr::is_state_key(key) {
        debug_assert!(
            false,
            "state-class queryable {key} declared through serve_queryable — its replies seed \
             the LWW plane and MUST carry an HLC timestamp (RFC 04 §3.2, #782). Use \
             served::serve_state_queryable."
        );
        tracing::warn!(
            key = %key,
            "state-class queryable declared through the unstamped seam; its seed replies \
             cannot be LWW-ordered against live samples (RFC 04 §3.2, #782)"
        );
    }
    // #957: a WRITE procedure's outcome must reach the host's audit trail, and
    // this seam hands out a bare `Query` whose `reply`/`reply_err` are the two
    // ways to answer without recording anything. `serve_write_queryable` has no
    // such path — its terminators record first — so route writes there.
    //
    // The classification is not a guess: it is the registry's own
    // `kind = "write"` column, read out of the compiled slice.
    if let Some((producer, path)) = crate::audit::rpc_route(key)
        && crate::audit::is_write_procedure(&producer, &path)
    {
        debug_assert!(
            false,
            "write procedure {key} declared through the unaudited seam — both of its outcomes \
             MUST reach the audit trail (#957). Use served::serve_write_queryable."
        );
        tracing::warn!(
            key = %key,
            "write procedure served through the unaudited seam; its outcomes will not reach \
             the host's audit trail (#957)"
        );
    }
    let queryable = session.declare_queryable(key).await?;
    note_served(key);
    Ok(queryable)
}

/// A queryable on a **write** procedure, whose outcomes are audited (#957).
///
/// # Why this is a separate type
///
/// SYS-SUP-019 asks for a journal of every operator action. A convention —
/// "call `audit::record` on both arms" — is a comment, and this module already
/// records twice over what comments cost here: #484 exists because seven
/// surfaces were advertised and not served, and [`StateQueryable`] exists
/// because the reply-stamping rule was obeyed until the next call site.
///
/// So [`WriteQuery`] exposes **no** `reply` and **no** `reply_err`. The only
/// two ways to answer are [`WriteQuery::executed`] and
/// [`WriteQuery::refused`], and both write the record before they reply — an
/// unaudited answer to a write procedure is not something a caller can spell.
///
/// The record goes out *before* the reply on purpose: a lost reply is a retry,
/// a lost record is a hole in the trail.
pub struct WriteQueryable {
    inner: zenoh::query::Queryable<zenoh::handlers::FifoChannelHandler<zenoh::query::Query>>,
    procedure: String,
    key: String,
}

/// One call on a write procedure. See [`WriteQueryable`].
pub struct WriteQuery {
    inner: zenoh::query::Query,
    procedure: String,
}

/// Declare a write procedure's queryable, recorded as served *and* as audited.
///
/// Refuses a key the registry does not declare `kind = "write"`: this seam
/// writes an audit record for every answer, and recording a read would bury
/// the actions in the noise the trail exists to avoid.
pub async fn serve_write_queryable(
    session: &zenoh::Session,
    key: &str,
) -> zenoh::Result<WriteQueryable> {
    let route = crate::audit::rpc_route(key);
    // Only a producer this build HAS a slice for can be classified. The
    // framework's artifact channel is generic over the producer name, so a
    // synthetic one (a test rig) resolves to no slice — "cannot say" is not
    // the same finding as "declared a read", and only the second is a bug.
    let classifiable = route
        .as_ref()
        .is_some_and(|(p, _)| crate::registry::registry_toml(p).is_some());
    debug_assert!(
        !classifiable
            || route
                .as_ref()
                .is_some_and(|(p, path)| crate::audit::is_write_procedure(p, path)),
        "serve_write_queryable called with {key}, which the registry does not declare as a \
         write procedure — reads are deliberately not audited (#957)"
    );
    let procedure = route
        .map(|(p, path)| format!("{p}/{path}"))
        .unwrap_or_else(|| key.to_string());
    let inner = session.declare_queryable(key).await?;
    note_served(key);
    audited()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string());
    Ok(WriteQueryable {
        inner,
        procedure,
        key: key.to_string(),
    })
}

fn audited() -> &'static Mutex<HashSet<String>> {
    static AUDITED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    AUDITED.get_or_init(Default::default)
}

impl WriteQueryable {
    /// The key this queryable answers.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Await the next call. `Err` when the session has closed.
    pub async fn recv_async(&self) -> zenoh::Result<WriteQuery> {
        self.inner.recv_async().await.map(|inner| WriteQuery {
            inner,
            procedure: self.procedure.clone(),
        })
    }

    /// Stop answering.
    pub async fn undeclare(self) -> zenoh::Result<()> {
        self.inner.undeclare().await
    }
}

impl WriteQuery {
    /// The call, with whatever the bus can say about its caller.
    pub fn request(&self) -> crate::rpc::RpcRequest {
        crate::rpc::RpcRequest::from_query(&self.inner)
    }

    /// The selector's parameters, for a procedure that takes some.
    pub fn parameters(&self) -> &zenoh::query::Parameters<'static> {
        self.inner.parameters()
    }

    /// The procedure this call is on, as it appears in an audit record.
    pub fn procedure(&self) -> &str {
        &self.procedure
    }

    /// Answer with the outcome of a permitted action.
    ///
    /// `target` is what was acted on — the unit, the outlet, the topic — and
    /// belongs in the record even when it is already in the payload, because
    /// the trail is read without the payload.
    pub async fn executed(
        self,
        reply_key: &str,
        payload: impl Into<zenoh::bytes::ZBytes>,
        target: Option<&str>,
    ) -> zenoh::Result<()> {
        self.executed_but(reply_key, payload, target, None).await
    }

    /// Answer with the outcome of a permitted action that did not achieve what
    /// it was asked to. The verdict is still `executed` — the gate said yes and
    /// the producer acted — with the failure recorded in `error`.
    pub async fn executed_but(
        self,
        reply_key: &str,
        payload: impl Into<zenoh::bytes::ZBytes>,
        target: Option<&str>,
        error: Option<String>,
    ) -> zenoh::Result<()> {
        let mut rec = crate::audit::AuditRecord::executed(&self.procedure)
            .with_request(&self.request())
            .with_error(error);
        rec.target = target.map(str::to_string);
        crate::audit::record(&rec);
        self.inner.reply(reply_key, payload).await
    }

    /// Refuse the call, recording which switch refused it.
    ///
    /// `refused_by` comes from the error's own field when the gate set one
    /// (#866); failing that, the error *name*, which is at least a true
    /// statement about the class that refused.
    pub async fn refused(
        self,
        err: &crate::rpc::RpcError,
        target: Option<&str>,
    ) -> zenoh::Result<()> {
        let switch = err.refused_by.clone().unwrap_or_else(|| err.error.clone());
        let mut rec = crate::audit::AuditRecord::refused(&self.procedure, switch)
            .with_request(&self.request());
        rec.target = target.map(str::to_string);
        rec.error = Some(err.message.clone());
        crate::audit::record(&rec);
        let payload = serde_json::to_vec(err).unwrap_or_default();
        self.inner.reply_err(payload).await
    }
}

/// Assert that every write procedure `producer` registers was declared through
/// the audited seam (#957).
///
/// The honest half of the enforcement: a test cannot watch a hook fire, but a
/// *declaration* is an observable event this process makes once, at startup,
/// exactly like [`check_registry_coverage`] — which is where this is called
/// from, so every sensor gets it without a new line.
pub fn check_write_coverage(producer: &str) {
    let Some(toml) = crate::registry::registry_toml(producer) else {
        return;
    };
    let Ok(slice) = zenkey::parse_slice(toml) else {
        return;
    };
    let audited = audited().lock().unwrap_or_else(|e| e.into_inner());
    let served = served().lock().unwrap_or_else(|e| e.into_inner());
    let missing: Vec<String> = slice
        .procedures
        .iter()
        .filter(|p| crate::audit::is_write_procedure(producer, &p.path))
        .map(|p| serve_spelling(producer, &p.path))
        // Only a procedure this build actually serves: an undeclared one is
        // already #484's finding, and reporting it twice buries the new one.
        .filter(|key| served.contains(key) && !audited.contains(key))
        .collect();
    if missing.is_empty() {
        return;
    }
    let list = missing.join(", ");
    debug_assert!(
        false,
        "{producer} serves write procedures through the unaudited seam: {list}. Both outcomes          of a write MUST reach the host's audit trail (#957) — declare them with          served::serve_write_queryable."
    );
    tracing::warn!(
        producer = %producer,
        unaudited = %list,
        "write procedures served without an audit trail (#957)"
    );
}

/// A queryable on a **state-class** selector, whose replies are stamped (#782).
///
/// # Why this is a separate type
///
/// RFC 04 §3.2 makes a producer answering a plain GET on a state selector a
/// *storage* for the duration of that reply — the reply-key discipline is
/// storage-shaped on purpose, so seeding works with or without a real one. And
/// §3.2 closes with the corollary: *"state publishers and storages MUST run
/// timestamped — an untimestamped sample cannot be reconciled."*
///
/// Zenoh's session HLC stamps a `put`. **It does not stamp a queryable reply.**
/// So for as long as the seed path went through the ordinary
/// [`serve_queryable`] seam, every seeded state sample arrived with no
/// timestamp: a consumer could not LWW-order it against a live sample, and
/// `zenctl doctor --deep` reported `unstamped-state` against the deployment.
///
/// The fix is a type rather than a convention. [`StateQuery`] exposes **no**
/// `reply()` — only [`StateQuery::reply_state`], which takes a stamp — so an
/// unstamped state seed is not something a caller can write through this seam.
/// A convention would have been a comment, and the comment would have been
/// obeyed until the next call site.
///
/// `@rpc` replies keep the ordinary seam, deliberately. They are computed
/// answers to parameterised questions, never the value at a key; no storage
/// selector reaches them and nothing merges them into an LWW store. Stamping
/// them would invite a consumer to cache them as state.
pub struct StateQueryable {
    inner: zenoh::query::Queryable<zenoh::handlers::FifoChannelHandler<zenoh::query::Query>>,
    key: String,
}

/// One GET against a state-class selector. See [`StateQueryable`].
pub struct StateQuery(zenoh::query::Query);

/// Declare a state-class seed queryable, recorded as served like any other.
///
/// Refuses a non-state selector: this seam stamps, and stamping an `@rpc`
/// reply is as wrong as not stamping a state one.
pub async fn serve_state_queryable(
    session: &zenoh::Session,
    selector: &str,
) -> zenoh::Result<StateQueryable> {
    debug_assert!(
        crate::keyexpr::is_state_key(selector),
        "serve_state_queryable called with a non-state selector {selector} — an @rpc reply is \
         a computed answer, not the value at a key, and must NOT be stamped (#782)"
    );
    let inner = session.declare_queryable(selector).await?;
    note_served(selector);
    Ok(StateQueryable {
        inner,
        key: selector.to_string(),
    })
}

/// The HLC stamp for one seed batch.
///
/// **Take this inside the same critical section as the snapshot it describes.**
/// Stamping at reply time instead is a resurrection bug: a seed loop snapshots
/// and *then* replies, so a value updated mid-loop has its live `put` stamped
/// `T` while the loop replies the stale snapshot value stamped `T' > T`, and
/// LWW picks the stale one. Taking the stamp with the snapshot closes that
/// window — every mutation not visible in the snapshot is `put` strictly later
/// and correctly wins.
///
/// Total by construction: with timestamping off, zenoh falls back to wall clock
/// plus the session's zid, so a seed reply is stamped unconditionally — a
/// stronger guarantee than `put` gives. (ZenSight forces timestamping on in
/// [`crate::session`] anyway.)
pub fn seed_stamp(session: &zenoh::Session) -> zenoh::time::Timestamp {
    session.new_timestamp()
}

impl StateQueryable {
    /// The selector this queryable answers.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Await the next GET. `Err` when the session has closed.
    pub async fn recv_async(&self) -> zenoh::Result<StateQuery> {
        self.inner.recv_async().await.map(StateQuery)
    }

    /// Stop answering.
    pub async fn undeclare(self) -> zenoh::Result<()> {
        self.inner.undeclare().await
    }
}

impl StateQuery {
    /// The selector's parameters, for a seed that takes one.
    pub fn parameters(&self) -> &zenoh::query::Parameters<'static> {
        self.0.parameters()
    }

    /// The key expression this GET asked for.
    pub fn key_expr(&self) -> &zenoh::key_expr::KeyExpr<'static> {
        self.0.key_expr()
    }

    /// Reply with one state document on its concrete key, stamped.
    ///
    /// `stamp` comes from [`seed_stamp`] and is shared by every reply in one
    /// batch — see that function for why it must be taken with the snapshot.
    pub async fn reply_state(
        &self,
        key: &str,
        payload: impl Into<zenoh::bytes::ZBytes>,
        stamp: zenoh::time::Timestamp,
    ) -> zenoh::Result<()> {
        debug_assert!(
            crate::keyexpr::is_state_key(key),
            "reply_state called with a non-state reply key {key} (#782)"
        );
        // `.timestamp()` is `TimestampBuilderTrait`, which zenoh marks
        // `#[zenoh_macros::internal_trait]` — a macro whose documented purpose
        // is to ALSO emit an inherent method, so no import and no `internal`
        // cargo feature is needed. `state_reply_builder_still_takes_a_timestamp`
        // below is the compile-level pin: if upstream ever drops that shim,
        // this turns into a named build failure rather than silently losing
        // the stamp again.
        self.0.reply(key, payload).timestamp(Some(stamp)).await
    }

    /// Refuse the GET.
    pub async fn reply_err(&self, payload: impl Into<zenoh::bytes::ZBytes>) -> zenoh::Result<()> {
        self.0.reply_err(payload).await
    }
}

/// Declare `keys` and answer `err` on every call, until the session closes.
///
/// The counterpart to [`serve_queryable`] for a surface this build advertises
/// but cannot currently serve. A registry slice lists a producer's procedures
/// unconditionally and `introspect` hands that slice to the fleet as truth, so
/// a procedure whose declaration sits behind a `#[cfg]`, a config flag, or a
/// capability the host lacks is a lie the moment the build ships without it —
/// and, since #484, a `debug_assert!` that kills the sensor at startup.
///
/// Answering an error rather than declaring nothing is what keeps three cases
/// apart for a caller (#648):
///
/// | what the caller sees | what it means |
/// |---|---|
/// | no reply at all | no such producer on the bus |
/// | `error/unsupported` | producer present, capability not in this build → rebuild |
/// | `error/gated` | capability built in, switched off here → reconfigure |
/// | an empty value reply | capability live, nothing to report |
///
/// Declaring nothing collapses the middle two into the first, which is exactly
/// the silence the registry check exists to prevent.
///
/// A **write** procedure declared here answers through the audited seam
/// ([`serve_write_queryable`]): a caller turned away by a shut gate is exactly
/// the event SYS-SUP-019 asks to be journalled (#957).
pub async fn serve_unavailable(
    session: std::sync::Arc<zenoh::Session>,
    keys: Vec<String>,
    err: crate::rpc::RpcError,
) {
    let payload = match serde_json::to_vec(&err) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "serve_unavailable: serialize error payload failed");
            return;
        }
    };
    let mut tasks = Vec::with_capacity(keys.len());
    for key in keys {
        let name = err.error.clone();
        tracing::debug!(key = %key, reason = %name, "procedure declared but unavailable");

        // A shut gate on a WRITE procedure is the most interesting refusal
        // there is — somebody tried to change something and the switch was off
        // — so it goes through the audited seam like any other refusal (#957),
        // rather than being the one refusal the trail never sees.
        if crate::audit::rpc_route(&key)
            .is_some_and(|(p, path)| crate::audit::is_write_procedure(&p, &path))
        {
            let queryable = match serve_write_queryable(&session, &key).await {
                Ok(q) => q,
                Err(e) => {
                    tracing::error!(error = %e, key = %key, "serve_unavailable: declare failed");
                    continue;
                }
            };
            let err = err.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(query) = queryable.recv_async().await {
                    if let Err(e) = query.refused(&err, None).await {
                        tracing::warn!(error = %e, key = %key, "unavailable reply failed");
                    }
                }
            }));
            continue;
        }

        let queryable = match serve_queryable(&session, &key).await {
            Ok(q) => q,
            Err(e) => {
                tracing::error!(error = %e, key = %key, "serve_unavailable: declare failed");
                continue;
            }
        };
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                if let Err(e) = query.reply_err(payload.clone()).await {
                    tracing::warn!(error = %e, key = %key, "unavailable reply failed");
                }
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

/// Record `key` as served without declaring it — for the paths that build
/// their queryable through another API (e.g. a zenoh-ext advanced queryable)
/// and would otherwise look unserved.
pub fn note_served(key: &str) {
    served()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string());
    served_changed().notify_waiters();
}

/// Whether `key` has been declared by this process.
pub fn is_served(key: &str) -> bool {
    served()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(key)
}

/// Every `@rpc` procedure `producer`'s registry slice declares but this build
/// does not serve (#484). Empty is the only honest state at `introspect` time.
///
/// Matching is on the **serve-side spelling**: a procedure with a `{var}`
/// chunk is served as a `*` wildcard, so that is what the served set holds.
pub fn unserved_procedures(producer: &str) -> Vec<String> {
    let Some(toml) = crate::registry::registry_toml(producer) else {
        return Vec::new();
    };
    let Ok(slice) = zenkey::parse_slice(toml) else {
        return Vec::new();
    };
    let served = served().lock().unwrap_or_else(|e| e.into_inner());
    slice
        .procedures
        .iter()
        .filter(|p| {
            let key = serve_spelling(producer, &p.path);
            !served.contains(&key)
        })
        .map(|p| p.path.clone())
        .collect()
}

/// The key a producer serves a procedure on: base-relative, own origin,
/// `{var}` chunks widened to `*` (the serve-side selector, RFC 05 §2).
fn serve_spelling(producer: &str, path: &str) -> String {
    use zenkey::ConcreteOrigin;
    let origin = crate::PROFILE.local_origin();
    let mut key = format!("v1/{}/@rpc/{producer}", origin.chunk());
    for chunk in path.split('/') {
        key.push('/');
        if chunk.starts_with('{') {
            key.push('*');
        } else {
            key.push_str(chunk);
        }
    }
    key
}

/// Assert that `producer` serves everything its registry slice advertises
/// (#484). Call once, at the point the producer starts serving `introspect` —
/// after its procedures are declared, before `alive` says it is callable.
pub fn check_registry_coverage(producer: &str) {
    // The write half rides along (#957): both checks answer "does what this
    // build advertises match what it actually declared", and both are only
    // meaningful at exactly this moment — after the procedures are declared,
    // before `alive` says the producer is callable.
    check_write_coverage(producer);
    let missing = unserved_procedures(producer);
    if missing.is_empty() {
        return;
    }
    let list = missing.join(", ");
    debug_assert!(
        false,
        "registry advertises procedures {list} that this build of `{producer}` does not serve — \
         `introspect` would ship them to the fleet as truth (RFC 08 §6.1, issue #484). Serve \
         them, or remove them from zensight-common/registry/{producer}.toml"
    );
    tracing::warn!(
        producer = %producer,
        unserved = %list,
        "registry advertises procedures this build does not serve — introspect is lying (RFC 08 §6.1)"
    );
}

/// Wait up to `grace` for `producer` to serve everything its registry slice
/// advertises, then report whatever gap is left ([`check_registry_coverage`]).
///
/// Sensors declare their queryables inside tasks spawned by
/// `SensorRunner::spawn` — a bare `tokio::spawn` — so sampling the served set
/// once from `run()` races them. Before this, the check passed only because two
/// intervening `.await`s (`serve_introspect`, `serve_describe`) happened to
/// yield long enough for those tasks to reach their declarations: a scheduling
/// accident dressed as a guarantee (#648).
///
/// The happy path costs nothing — the set is usually already closed, and
/// otherwise this returns the moment it closes. The grace is also the
/// *contract*, not just a timeout: RFC 04 §5 requires queryables declared
/// before the `alive` token, and this runs immediately before it. A procedure
/// declared later than `grace` is late by definition, so reporting it is
/// correct rather than a false positive.
pub async fn await_registry_coverage(producer: &str, grace: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        // Subscribe to the wakeup BEFORE re-reading the predicate. The other
        // order drops a `note_served` landing between the two, and the wait
        // then burns the whole grace for a set that had already closed.
        let changed = served_changed().notified();
        if unserved_procedures(producer).is_empty() {
            return;
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            break;
        }
    }
    check_registry_coverage(producer);
}

/// Wait until every key in `keys` has been declared through this module, or
/// `grace` elapses. Returns whatever is still missing (empty is success).
///
/// The origin-agnostic sibling of [`await_registry_coverage`], and it exists
/// because that one is **sensor-shaped**: it derives the serve-side spelling
/// from *this host's* origin (`v1/h-…/@rpc/<producer>/<proc>`), which is right
/// for a sensor and wrong for a producer on a **service** origin. The catalog
/// serves `v1/@catalog/@rpc/names`, so `unserved_procedures("catalog")` reports
/// every one of its procedures as missing even while the log says they are
/// ready — which is exactly what it did the first time the correlator tried to
/// use it.
///
/// So a caller on a service origin passes the concrete keys it declared. Same
/// bounded wait, same wakeup discipline, no assumption about how the key was
/// spelled.
pub async fn await_served(keys: &[String], grace: std::time::Duration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        // Subscribe to the wakeup BEFORE re-reading the predicate, for the
        // reason `await_registry_coverage` gives: the other order drops a
        // `note_served` landing between the two.
        let changed = served_changed().notified();
        let missing: Vec<String> = keys.iter().filter(|k| !is_served(k)).cloned().collect();
        if missing.is_empty() {
            return missing;
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return missing;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `await_served` returns once the keys land, and reports what did not.
    #[tokio::test]
    async fn await_served_waits_for_concrete_keys() {
        let key = "v1/@catalog/@rpc/await-served-test".to_string();
        let missing = await_served(
            std::slice::from_ref(&key),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert_eq!(missing, vec![key.clone()], "not served yet");

        note_served(&key);
        assert!(
            await_served(&[key], std::time::Duration::from_millis(50))
                .await
                .is_empty(),
            "served now"
        );
    }

    /// The reason `await_served` exists: the coverage helper is sensor-shaped
    /// and cannot see a service origin's keys.
    #[test]
    fn registry_coverage_cannot_see_a_service_origin() {
        let served_side = serve_spelling("catalog", "names");
        assert!(
            served_side.starts_with("v1/h-"),
            "the coverage helper spells the LOCAL HOST origin: {served_side}"
        );
        assert_ne!(
            served_side,
            crate::keyexpr::names_query_key(),
            "…but the catalog serves on the @catalog service origin, so the two \
             never match — which is why a service-origin producer needs \
             await_served and not await_registry_coverage"
        );
    }

    /// The state/`@rpc` split this module's stamping rule turns on (#782).
    ///
    /// Not a test of `is_state_key` — that lives in `keyexpr` — but of the
    /// *classification* the two seams disagree on, spelled with the real key
    /// shapes both call sites produce. If either of these flips, one seam
    /// starts refusing what the other requires.
    #[test]
    fn the_two_seed_selectors_are_state_and_every_rpc_key_is_not() {
        // The two state-class seeds, and the only two in the workspace.
        assert!(crate::keyexpr::is_state_key(
            "v1/@catalog/state/entity/h-0123456789ab"
        ));
        assert!(crate::keyexpr::is_state_key(
            "v1/h-0123456789ab/state/sysinfo/alert/a659f813308ad1da"
        ));
        // The service origin matters: an origin gate would exempt @catalog,
        // which is the family that found this bug.
        assert!(crate::keyexpr::is_state_key(
            "v1/@catalog/state/alias/h-dead"
        ));

        // Everything else replies on @rpc, and must NOT be stamped.
        for rpc in [
            "v1/h-0123456789ab/@rpc/netring/flows",
            "v1/h-0123456789ab/@rpc/sysinfo/processes",
            "v1/@catalog/@rpc/catalog/names",
            "v1/h-0123456789ab/@rpc/parallax/stream/set",
        ] {
            assert!(
                !crate::keyexpr::is_state_key(rpc),
                "{rpc} must not be state"
            );
        }
    }

    /// A compile-level pin on the one upstream detail `reply_state` leans on.
    ///
    /// `ReplyBuilder`'s `.timestamp()` comes from `TimestampBuilderTrait`,
    /// which zenoh annotates `#[zenoh_macros::internal_trait]` — a macro whose
    /// documented job is to *also* emit an inherent method, so we need neither
    /// the import nor zenoh's `internal` cargo feature. That is a convenience
    /// shim, not a stability promise: if a zenoh upgrade drops it, we want a
    /// named build failure here rather than a silent return to unstamped
    /// seeds, which is the exact state #782 exists to leave behind.
    ///
    /// (This is a *compile* assertion. It never runs the closure — building
    /// a real `Query` needs a session and a live GET, which the e2e tests do.)
    #[test]
    fn state_reply_builder_still_takes_a_timestamp() {
        #[allow(dead_code)]
        async fn pin(query: &zenoh::query::Query, stamp: zenoh::time::Timestamp) {
            let _ = query
                .reply("v1/@catalog/state/entity/x", Vec::<u8>::new())
                .timestamp(Some(stamp))
                .await;
        }
    }

    /// A procedure is "served" under its serve-side spelling — `{var}` chunks
    /// widened to `*`, which is what a producer actually declares.
    #[test]
    fn serve_spelling_widens_vars() {
        let spelled = serve_spelling("netring", "capture/{ulid}");
        assert!(spelled.ends_with("/@rpc/netring/capture/*"), "{spelled}");
        assert!(spelled.starts_with("v1/h-"), "own origin, base-relative");

        let plain = serve_spelling("netring", "flows");
        assert!(plain.ends_with("/@rpc/netring/flows"), "{plain}");
    }

    /// The guard reports exactly what the build failed to serve, and goes
    /// quiet once the gap is closed.
    #[test]
    fn coverage_reports_only_the_gap() {
        // An unknown producer has no slice to lie about.
        assert!(unserved_procedures("not-a-producer").is_empty());

        // A real producer with nothing served yet: every declared procedure
        // is missing (this is the state the #453 audit shipped in).
        let missing = unserved_procedures("catalog");
        assert!(
            missing.iter().any(|p| p == "names"),
            "expected catalog/names among {missing:?}"
        );

        // Serve one, and only it drops out of the report.
        note_served(&serve_spelling("catalog", "names"));
        let after = unserved_procedures("catalog");
        assert!(!after.iter().any(|p| p == "names"));
        assert!(
            after.len() == missing.len() - 1,
            "serving one procedure closes exactly one gap"
        );
    }

    /// The wait returns as soon as a late declaration closes the gap, rather
    /// than sleeping out the grace — the whole point of the `Notify` (#648).
    ///
    /// `parallax` is used here (not `catalog`, which the test above mutates)
    /// because the served set is process-global and shared across this binary.
    #[tokio::test]
    async fn await_returns_when_a_late_declaration_lands() {
        let missing = unserved_procedures("parallax");
        assert!(
            !missing.is_empty(),
            "fixture producer must start incomplete"
        );

        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            for p in unserved_procedures("parallax") {
                note_served(&serve_spelling("parallax", &p));
            }
        });

        let t0 = std::time::Instant::now();
        await_registry_coverage("parallax", std::time::Duration::from_secs(30)).await;
        let waited = t0.elapsed();

        assert!(
            unserved_procedures("parallax").is_empty(),
            "returned before the gap closed"
        );
        assert!(
            waited >= std::time::Duration::from_millis(40),
            "did not wait"
        );
        assert!(
            waited < std::time::Duration::from_secs(5),
            "burned the grace instead of waking on the notify: {waited:?}"
        );
    }

    /// A `note_served` landing between the predicate read and the wakeup
    /// subscription must not cost the full grace. Subscribing first is what
    /// makes that true; this pins it.
    #[tokio::test]
    async fn a_notification_racing_the_check_is_not_lost() {
        let procs = unserved_procedures("modbus");
        if procs.is_empty() {
            return; // nothing to race against in this build
        }
        // Hammer the set from another task while the waiter spins.
        tokio::spawn(async move {
            for p in procs {
                tokio::task::yield_now().await;
                note_served(&serve_spelling("modbus", &p));
            }
        });
        let t0 = std::time::Instant::now();
        await_registry_coverage("modbus", std::time::Duration::from_secs(30)).await;
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "a lost wakeup made the waiter sit out its grace"
        );
    }
}
