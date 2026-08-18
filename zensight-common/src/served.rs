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
    let queryable = session.declare_queryable(key).await?;
    note_served(key);
    Ok(queryable)
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
        let queryable = match serve_queryable(&session, &key).await {
            Ok(q) => q,
            Err(e) => {
                tracing::error!(error = %e, key = %key, "serve_unavailable: declare failed");
                continue;
            }
        };
        let payload = payload.clone();
        let name = err.error.clone();
        tracing::debug!(key = %key, reason = %name, "procedure declared but unavailable");
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

#[cfg(test)]
mod tests {
    use super::*;

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
