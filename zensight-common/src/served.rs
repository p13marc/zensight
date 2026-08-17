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

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

fn served() -> &'static Mutex<HashSet<String>> {
    static SERVED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SERVED.get_or_init(Default::default)
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

/// Record `key` as served without declaring it — for the paths that build
/// their queryable through another API (e.g. a zenoh-ext advanced queryable)
/// and would otherwise look unserved.
pub fn note_served(key: &str) {
    served()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string());
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
}
