use zenoh::Session;
use zenoh::key_expr::KeyExpr;

use crate::config::ZenohConfig;
use crate::error::{Error, Result};

/// Build the `zenoh::Config` every ZenSight participant opens.
///
/// **This is the one place the deployment base is spelled** (RFC 09 §0, issue
/// #466). Every key the application builds is base-relative (`v1/…`); the
/// session `namespace` prefixes the base on egress, strips it on ingress, and
/// filters ingress from outside it.
///
/// Exposed separately from [`connect`] so that a caller needing an extra knob
/// (the isolated-run configs in `zensight-rerun` and the e2e tests) can start
/// from the shared config rather than rebuilding it — a second hand-rolled
/// `zenoh::Config` is a session that silently misses the namespace.
pub fn build_config(config: &ZenohConfig) -> Result<zenoh::Config> {
    let mut zenoh_config = zenoh::Config::default();

    // Set mode
    let mode_str = match config.mode.as_str() {
        "client" | "peer" | "router" => format!("\"{}\"", config.mode),
        other => {
            return Err(Error::Config(format!(
                "Invalid Zenoh mode: '{}'. Expected 'client', 'peer', or 'router'",
                other
            )));
        }
    };

    zenoh_config
        .insert_json5("mode", &mode_str)
        .map_err(|e| Error::Config(format!("Failed to set mode: {}", e)))?;

    // The deployment base as the session namespace (RFC 03 §1.1, 09 §0).
    //
    // An empty namespace is not "no base" — it is a session that publishes
    // base-relative keys (`v1/…`) at the *bus root*, which is a different
    // keyspace from `zensight/v1/…`, invisible to every other participant, and
    // silent about it. That failure has no error and no symptom except missing
    // data, so it is rejected here instead.
    let ns = config.namespace.trim();
    if ns.is_empty() {
        return Err(Error::Config(
            "zenoh.namespace must not be empty — it is the deployment base (default \"zensight\"). \
             An empty namespace publishes base-relative keys at the bus root, where nothing is \
             listening. Debug tools that want the un-namespaced wire view do not use this helper \
             (RFC 09 §5)."
                .into(),
        ));
    }
    // Zenoh requires a non-wild keyexpr. A wildcard here would be accepted by
    // `insert_json5` and fail later, at declare time, as a session that matches
    // nothing.
    let parsed = KeyExpr::try_from(ns)
        .map_err(|e| Error::Config(format!("invalid zenoh.namespace {ns:?}: {e}")))?;
    if parsed.is_wild() {
        return Err(Error::Config(format!(
            "zenoh.namespace must not contain wildcards: {ns:?}"
        )));
    }
    zenoh_config
        .insert_json5("namespace", &format!("{ns:?}"))
        .map_err(|e| Error::Config(format!("Failed to set namespace: {}", e)))?;

    // Set connect endpoints
    if !config.connect.is_empty() {
        let endpoints_json = serde_json::to_string(&config.connect)
            .map_err(|e| Error::Config(format!("Failed to serialize connect endpoints: {}", e)))?;

        zenoh_config
            .insert_json5("connect/endpoints", &endpoints_json)
            .map_err(|e| Error::Config(format!("Failed to set connect endpoints: {}", e)))?;
    }

    // Set listen endpoints
    if !config.listen.is_empty() {
        let endpoints_json = serde_json::to_string(&config.listen)
            .map_err(|e| Error::Config(format!("Failed to serialize listen endpoints: {}", e)))?;

        zenoh_config
            .insert_json5("listen/endpoints", &endpoints_json)
            .map_err(|e| Error::Config(format!("Failed to set listen endpoints: {}", e)))?;
    }

    // zenoh-ext AdvancedPublisher caches sequence by timestamp, which requires
    // session timestamping — zenoh enables it by default only for routers, so
    // without this every peer/client-mode sensor fails to create its cached
    // identity/evidence/registration publishers ("the 'timestamping' setting
    // must be enabled") and the correlator never receives evidence.
    zenoh_config
        .insert_json5("timestamping/enabled", "true")
        .map_err(|e| Error::Config(format!("Failed to enable timestamping: {}", e)))?;

    // Isolated runs (tests, smoke, pinned-endpoint deployments) turn
    // multicast scouting off so the session can NEVER join a mesh beyond its
    // explicit endpoints. Gossip stays on: it only propagates within the
    // already-connected graph, and hub-and-spoke deployments rely on it
    // (RFC 09 §0.1 — the two are independent switches, and disabling both is
    // what silently breaks spoke-to-spoke discovery).
    if !config.scouting {
        zenoh_config
            .insert_json5("scouting/multicast/enabled", "false")
            .map_err(|e| Error::Config(format!("Failed to disable multicast scouting: {}", e)))?;
    }

    Ok(zenoh_config)
}

/// Connect to Zenoh using the provided configuration.
pub async fn connect(config: &ZenohConfig) -> Result<Session> {
    // Honor ZENSIGHT_ZENOH_* env overrides (e.g. set by `just run` to pin a
    // local rendezvous endpoint instead of relying on multicast discovery).
    let config = &config.clone().with_env_overrides();
    let zenoh_config = build_config(config)?;

    tracing::info!(
        mode = %config.mode,
        connect = ?config.connect,
        listen = ?config.listen,
        scouting = config.scouting,
        namespace = %config.namespace,
        "Connecting to Zenoh"
    );

    let session = zenoh::open(zenoh_config).await?;

    tracing::info!(zid = %session.zid(), "Connected to Zenoh");

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(namespace: &str) -> ZenohConfig {
        ZenohConfig {
            namespace: namespace.to_string(),
            ..Default::default()
        }
    }

    /// The whole point of #466: the base reaches the wire as session config,
    /// not as a string any application key contains.
    #[test]
    fn the_namespace_is_set_from_config() {
        let c = build_config(&cfg("zensight")).expect("default config builds");
        let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
        assert_eq!(json["namespace"], "zensight");
        assert_eq!(json["timestamping"]["enabled"], true);
    }

    /// Multi-chunk bases are legal (RFC 03 §1.1) and are now purely config.
    #[test]
    fn a_multi_chunk_base_is_a_legal_namespace() {
        let c = build_config(&cfg("acme/fleet-a")).expect("multi-chunk base builds");
        let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
        assert_eq!(json["namespace"], "acme/fleet-a");
    }

    /// An empty namespace would publish `v1/…` at the bus root — a different
    /// keyspace, invisible to everyone, with no error at declare or publish
    /// time. Refuse it at startup, where it is still debuggable.
    #[test]
    fn an_empty_namespace_is_refused() {
        let err = build_config(&cfg("   ")).expect_err("empty namespace must not build");
        assert!(
            err.to_string().contains("must not be empty"),
            "unhelpful error: {err}"
        );
    }

    /// A wildcard namespace is accepted by `insert_json5` and only fails later,
    /// as a session that matches nothing. Catch it here.
    #[test]
    fn a_wildcard_namespace_is_refused() {
        for bad in ["zensight/*", "**", "*"] {
            let err = build_config(&cfg(bad)).expect_err("wildcard namespace must not build");
            assert!(
                err.to_string().contains("wildcard") || err.to_string().contains("invalid"),
                "unhelpful error for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn a_bad_mode_is_refused() {
        let c = ZenohConfig {
            mode: "nonsense".into(),
            ..Default::default()
        };
        assert!(build_config(&c).is_err());
    }
}
