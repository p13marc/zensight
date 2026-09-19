//! A target may name its own trust anchor (#1136).
//!
//! The root store was `webpki_roots::TLS_SERVER_ROOTS` and **nothing else**, so
//! every internal-CA endpoint was a permanent critical: a `tls` target
//! published `chain_valid: false` and fired
//! `probe-certificate-chain-invalid` every sweep, and an `http` target failed
//! the handshake outright.
//!
//! That included **the ZenSight mesh certificates this crate's README says it
//! exists to watch** — "retires the monthly cron that warns when a ZenSight
//! mesh certificate is within 60 days of expiry" — and a mesh certificate is
//! signed by an internal CA by definition. The one escape,
//! `alerts.chain_invalid: false`, is global.
//!
//! A real CA and a real handshake, because the claim is about what rustls
//! believes and a fixture PEM cannot demonstrate that.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::probe::ProbeKind;
use zensight_sensor_probe::config::{ProbeAlertsConfig, Target};
use zensight_sensor_probe::tls::{TlsIdentity, inspect_socket};

/// The `main.rs` line, because a test binary is its own process and rustls
/// resolves the provider once per process.
fn install_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// An internal CA and a `localhost` server certificate signed by it.
fn internal_pki() -> (String, String, String) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "ZenSight Test Internal CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let mut leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    (ca.pem(), leaf.pem(), leaf_key.serialize_pem())
}

/// A TLS listener presenting `cert_pem`/`key_pem`. Returns its address.
async fn spawn_tls(cert_pem: String, key_pem: String) -> String {
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .unwrap()
        .unwrap();
    let cfg = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            // The handshake is the whole test; whatever happens after it does
            // not matter.
            tokio::spawn(async move {
                let _ = acceptor.accept(stream).await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_internal_ca_endpoint_validates_when_the_target_names_its_ca() {
    install_provider();
    let (ca_pem, leaf_pem, leaf_key) = internal_pki();
    let addr = spawn_tls(leaf_pem, leaf_key).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("internal-ca.pem");
    std::fs::write(&ca_file, &ca_pem).unwrap();

    // Without it: the permanent critical. This is what every mesh endpoint
    // looked like.
    let bare = inspect_socket(
        &addr,
        "localhost",
        Duration::from_secs(5),
        true,
        TlsIdentity::default(),
    )
    .await
    .expect("the handshake completes — inspect_untrusted reads without trusting");
    assert_eq!(
        bare.chain_valid,
        Some(false),
        "an internal CA is not in webpki_roots, and that was the whole problem"
    );

    // With it: the same endpoint, the same certificate, and a valid chain.
    let trusted = inspect_socket(
        &addr,
        "localhost",
        Duration::from_secs(5),
        true,
        TlsIdentity {
            ca_file: ca_file.to_str(),
            ..Default::default()
        },
    )
    .await
    .expect("handshake");
    assert_eq!(
        trusted.chain_valid,
        Some(true),
        "naming the CA is what makes the chain valid"
    );
    assert_eq!(
        trusted.san_matched,
        Some(true),
        "and the SAN check is unaffected"
    );
    assert_eq!(trusted.subject, bare.subject, "the same certificate");
}

/// The public roots are **added to, not replaced**.
///
/// A target behind an internal CA still needs them for anything in its
/// redirect chain — the rule the bmc client already follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naming_a_ca_does_not_drop_the_public_roots() {
    install_provider();
    let (ca_pem, _, _) = internal_pki();
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("internal-ca.pem");
    std::fs::write(&ca_file, &ca_pem).unwrap();

    let with_ca = zensight_sensor_probe::tls::root_store(ca_file.to_str()).unwrap();
    let without = zensight_sensor_probe::tls::root_store(None).unwrap();
    assert_eq!(
        with_ca.roots.len(),
        without.roots.len() + 1,
        "exactly one anchor added, and none taken away"
    );
}

/// A `ca_file` that is not a certificate is a **named** error, not a silent
/// fall-back to the public roots — which would look like the CA was trusted
/// and then fail the chain every sweep anyway.
#[test]
fn a_ca_file_that_holds_no_certificate_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("not-a-ca.pem");
    std::fs::write(
        &junk,
        b"-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let err = zensight_sensor_probe::tls::root_store(junk.to_str()).unwrap_err();
    assert!(err.contains("no CERTIFICATE block"), "{err}");
    assert!(err.contains("not-a-ca.pem"), "and it names the file: {err}");
}

fn target(name: &str) -> Target {
    Target {
        name: name.into(),
        kind: ProbeKind::Tls,
        target: "example.invalid:443".into(),
        interval_secs: None,
        timeout_secs: None,
        expect_status: vec![],
        expect_body: None,
        follow_redirects: true,
        allow_offhost_redirect: false,
        method: None,
        headers: vec![],
        count: None,
        spacing_ms: None,
        transport: None,
        server_name: None,
        inspect_untrusted: true,
        resolver: None,
        expect_addrs: vec![],
        ca_file: None,
        client_cert_file: None,
        client_key_file: None,
        chain_invalid_alert: None,
        enabled: true,
    }
}

/// The opt-out is **per target**, where it used to be global.
#[test]
fn one_target_can_opt_out_of_chain_invalid_without_silencing_the_rest() {
    use zensight_common::probe::{ProbeOutcome, ProbeResult, TlsResult};
    use zensight_sensor_probe::alerts::{Exemptions, grade};

    let result = |name: &str| ProbeResult {
        name: name.to_string(),
        kind: ProbeKind::Tls,
        target: "host:443".into(),
        outcome: ProbeOutcome::Ok,
        duration_ms: Some(1.0),
        error: None,
        http: None,
        tls: Some(TlsResult {
            chain_valid: Some(false),
            ..Default::default()
        }),
        dns: None,
        burst: None,
        ntp: None,
        vantage: "v".into(),
        observed_at_ms: 0,
    };
    let results = [result("self-signed-appliance"), result("mesh-node")];

    let none = grade(
        &ProbeAlertsConfig::default(),
        "host",
        &results,
        &Exemptions::default(),
    );
    assert_eq!(
        none.iter()
            .filter(|a| a.rule == "probe-certificate-chain-invalid")
            .count(),
        2,
        "both fire by default"
    );

    let mut exempt = target("self-signed-appliance");
    exempt.chain_invalid_alert = Some(false);
    let some = grade(
        &ProbeAlertsConfig::default(),
        "host",
        &results,
        &Exemptions::from_targets(&[exempt, target("mesh-node")]),
    );
    let still: Vec<&str> = some
        .iter()
        .filter(|a| a.rule == "probe-certificate-chain-invalid")
        .map(|a| a.labels.get("probe").map(String::as_str).unwrap_or(""))
        .collect();
    assert_eq!(
        some.iter()
            .filter(|a| a.rule == "probe-certificate-chain-invalid")
            .count(),
        1,
        "exempting one target must not silence the other — which is what the \
         global `alerts.chain_invalid: false` did, and it was the only escape \
         this sensor had: {still:?}"
    );
}
