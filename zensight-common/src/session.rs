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
    // The base names a *deployment*, not the software, so there is no
    // software default. An empty/unset namespace is the legal default and
    // matches Zenoh's own: no session namespace is set and the deployment's
    // keys live at the bus root (`v1/…`). Setting a base is the opt-in
    // isolation knob for running several deployments on one Zenoh
    // infrastructure — a mismatched base (empty vs. named, or two different
    // names) is the same partition either way: the sessions cannot see each
    // other.
    let ns = config.namespace.trim();
    if !ns.is_empty() {
        // Zenoh requires a non-wild keyexpr. A wildcard here would be accepted
        // by `insert_json5` and fail later, at declare time, as a session that
        // matches nothing.
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
    }

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

    // TLS material for `tls/…` endpoints, mapped onto Zenoh's
    // `transport/link/tls` block. ZenSight processes are TLS clients only —
    // the listening side is the zenohd router, configured in its own file —
    // so only the connect-side keys are set here.
    if let Some(tls) = &config.tls {
        if tls.connect_certificate.is_some() != tls.connect_private_key.is_some() {
            return Err(Error::Config(
                "zenoh.tls: connect_certificate and connect_private_key only make sense \
                 together — set both or neither"
                    .to_string(),
            ));
        }
        if tls.enable_mtls && tls.connect_certificate.is_none() {
            return Err(Error::Config(
                "zenoh.tls: enable_mtls requires connect_certificate and connect_private_key"
                    .to_string(),
            ));
        }
        let mut set = |key: &str, value: &Option<String>| -> Result<()> {
            if let Some(v) = value {
                zenoh_config
                    .insert_json5(&format!("transport/link/tls/{key}"), &format!("{v:?}"))
                    .map_err(|e| Error::Config(format!("Failed to set tls {key}: {e}")))?;
            }
            Ok(())
        };
        set("root_ca_certificate", &tls.root_ca_certificate)?;
        set("connect_certificate", &tls.connect_certificate)?;
        set("connect_private_key", &tls.connect_private_key)?;
        if tls.enable_mtls {
            zenoh_config
                .insert_json5("transport/link/tls/enable_mtls", "true")
                .map_err(|e| Error::Config(format!("Failed to set tls enable_mtls: {e}")))?;
        }
        // The classic silent misconfiguration: a tls block with only plain
        // `tcp/…` endpoints — the material is loaded and never used, and the
        // traffic stays cleartext.
        let uses_tls = config
            .connect
            .iter()
            .chain(config.listen.iter())
            .any(|e| e.starts_with("tls/") || e.starts_with("quic/"));
        if !uses_tls {
            tracing::warn!(
                "zenoh.tls is configured but no connect/listen endpoint uses a tls/ or quic/ \
                 scheme — the TLS material will not be used"
            );
        }
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
        tls = config.tls.is_some(),
        mtls = config.tls.as_ref().is_some_and(|t| t.enable_mtls),
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

    /// The empty base is the legal default (RFC 03 §1.1 as amended): no
    /// session namespace is set — matching Zenoh's own default — and the
    /// deployment's keys live at the bus root. Pin that nothing sneaks a
    /// namespace in.
    #[test]
    fn an_empty_namespace_sets_no_session_namespace() {
        for empty in ["", "   "] {
            let c = build_config(&cfg(empty)).expect("empty namespace is the legal default");
            let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
            assert_eq!(json["namespace"], serde_json::Value::Null, "for {empty:?}");
        }
        // The unset (Default) path is the same thing.
        build_config(&ZenohConfig::default()).expect("default config builds");
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

    fn tls_cfg(tls: crate::config::ZenohTlsConfig) -> ZenohConfig {
        ZenohConfig {
            mode: "client".into(),
            connect: vec!["tls/10.0.0.1:7447".into()],
            tls: Some(tls),
            ..Default::default()
        }
    }

    /// The TLS paths must reach the wire config under Zenoh's own key names —
    /// a typo'd key is silently ignored by zenoh and the link stays cleartext.
    #[test]
    fn tls_fields_reach_the_zenoh_config() {
        let c = build_config(&tls_cfg(crate::config::ZenohTlsConfig {
            root_ca_certificate: Some("/tls/ca.crt".into()),
            connect_certificate: Some("/tls/node.crt".into()),
            connect_private_key: Some("/tls/node.key".into()),
            enable_mtls: true,
        }))
        .expect("full tls block builds");
        let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
        let tls = &json["transport"]["link"]["tls"];
        assert_eq!(tls["root_ca_certificate"], "/tls/ca.crt");
        assert_eq!(tls["connect_certificate"], "/tls/node.crt");
        assert_eq!(tls["connect_private_key"], "/tls/node.key");
        assert_eq!(tls["enable_mtls"], true);
    }

    /// CA-only (server-authenticated TLS, no client cert) is a legal
    /// configuration and must not set the connect-side keys.
    #[test]
    fn ca_only_tls_is_legal() {
        let c = build_config(&tls_cfg(crate::config::ZenohTlsConfig {
            root_ca_certificate: Some("/tls/ca.crt".into()),
            ..Default::default()
        }))
        .expect("ca-only tls builds");
        let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
        let tls = &json["transport"]["link"]["tls"];
        assert_eq!(tls["root_ca_certificate"], "/tls/ca.crt");
        assert_eq!(tls["connect_certificate"], serde_json::Value::Null);
    }

    /// No tls block ⇒ no tls keys — the zenoh defaults stay untouched.
    #[test]
    fn no_tls_block_sets_no_tls_keys() {
        let c = build_config(&ZenohConfig::default()).expect("default config builds");
        let json: serde_json::Value = serde_json::from_str(&c.to_string()).unwrap();
        assert_eq!(
            json["transport"]["link"]["tls"]["root_ca_certificate"],
            serde_json::Value::Null
        );
        assert_eq!(
            json["transport"]["link"]["tls"]["enable_mtls"],
            serde_json::Value::Null
        );
    }

    /// mTLS without the client material would be accepted by zenoh and fail
    /// at handshake time on every connection attempt. Refuse it at build.
    #[test]
    fn mtls_without_client_cert_is_refused() {
        let err = build_config(&tls_cfg(crate::config::ZenohTlsConfig {
            root_ca_certificate: Some("/tls/ca.crt".into()),
            enable_mtls: true,
            ..Default::default()
        }))
        .expect_err("mtls without cert+key must not build");
        assert!(
            err.to_string().contains("enable_mtls"),
            "unhelpful error: {err}"
        );
    }

    /// A certificate without its key (or vice versa) is always a mistake.
    #[test]
    fn cert_without_key_is_refused() {
        for (cert, key) in [(Some("/tls/node.crt"), None), (None, Some("/tls/node.key"))] {
            let err = build_config(&tls_cfg(crate::config::ZenohTlsConfig {
                connect_certificate: cert.map(String::from),
                connect_private_key: key.map(String::from),
                ..Default::default()
            }))
            .expect_err("cert xor key must not build");
            assert!(
                err.to_string().contains("together"),
                "unhelpful error: {err}"
            );
        }
    }
}
