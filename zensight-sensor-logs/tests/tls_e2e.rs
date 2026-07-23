//! TLS syslog listener end-to-end tests (#550, RFC 5425). Real localhost TLS
//! handshakes against the running listener, no external services.

mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::*;
use tokio::io::AsyncWriteExt;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use zensight_sensor_logs::config::TlsListenerConfig;

const DEADLINE: Duration = Duration::from_secs(5);

/// Generate a self-signed server cert for `localhost`; return (cert_pem, key_pem).
fn gen_cert() -> (String, String) {
    let c = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (c.cert.pem(), c.signing_key.serialize_pem())
}

fn write_certs(dir: &std::path::Path) -> (TlsListenerConfig, Vec<u8>) {
    let (cert_pem, key_pem) = gen_cert();
    let cert = dir.join("server.crt");
    let key = dir.join("server.key");
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, &key_pem).unwrap();
    (
        TlsListenerConfig {
            cert_file: cert.to_string_lossy().into_owned(),
            key_file: key.to_string_lossy().into_owned(),
            client_ca_file: None,
            min_version: "1.3".into(),
        },
        cert_pem.into_bytes(),
    )
}

/// Build a client TLS connector trusting `server_cert_pem` (ring provider).
fn client_connector(server_cert_pem: &[u8]) -> TlsConnector {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &server_cert_pem[..]) {
        roots.add(cert.unwrap()).unwrap();
    }
    let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "zensight-tls-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// An octet-counted RFC 6587 frame: `MSG-LEN SP MSG`.
fn octet_frame(msg: &str) -> Vec<u8> {
    format!("{} {msg}", msg.len()).into_bytes()
}

/// A TLS client delivers an RFC-syslog line end-to-end; a cleartext connection
/// to the same port is rejected (no record).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_delivers_and_cleartext_is_rejected() {
    let dir = tempdir();
    let (tls_cfg, cert_pem) = write_certs(&dir);
    let port = free_tcp_port();
    let rig = RigBuilder::tls(port, tls_cfg).start().await;

    // TLS client: connect, octet-frame one line, close.
    let connector = client_connector(&cert_pem);
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let domain = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(domain, tcp).await.expect("tls handshake");
    tls.write_all(&octet_frame("<34>Oct 11 22:14:15 tlshost su: over tls"))
        .await
        .unwrap();
    tls.flush().await.unwrap();

    let records = rig.events_until(1, DEADLINE).await;
    assert_eq!(records.len(), 1, "the TLS-delivered line reaches the ring");
    assert_eq!(records[0].host, "tlshost");

    // Cleartext to the TLS port: the handshake fails, nothing is ingested.
    let mut plain = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let _ = plain
        .write_all(&octet_frame("<34>Oct 11 22:14:15 cleartext su: nope"))
        .await;
    let _ = plain.flush().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = rig.events("").await;
    assert!(
        !after.iter().any(|r| r.host == "cleartext"),
        "a cleartext connection to the TLS port must not be ingested"
    );
}

/// mTLS: with `client_ca_file` set, a client presenting no cert is refused —
/// the handshake fails, so nothing lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_refuses_client_without_cert() {
    let dir = tempdir();
    let (mut tls_cfg, cert_pem) = write_certs(&dir);
    // Require client certs (reuse the server cert as the client CA for the test).
    let ca = dir.join("ca.crt");
    std::fs::write(&ca, &cert_pem).unwrap();
    tls_cfg.client_ca_file = Some(ca.to_string_lossy().into_owned());

    let port = free_tcp_port();
    let rig = RigBuilder::tls(port, tls_cfg).start().await;

    // Client presents NO cert → server's verifier rejects at handshake.
    let connector = client_connector(&cert_pem);
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let domain = ServerName::try_from("localhost").unwrap();
    let mut sent = false;
    if let Ok(mut tls) = connector.connect(domain, tcp).await {
        // Some stacks complete the client side and fail on first write/read.
        sent = tls
            .write_all(&octet_frame("<34>Oct 11 22:14:15 nocert su: blocked"))
            .await
            .is_ok()
            && tls.flush().await.is_ok();
    }
    let _ = sent;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let records = rig.events("").await;
    assert!(
        !records.iter().any(|r| r.host == "nocert"),
        "a client without a valid cert must be refused under mTLS"
    );
}
