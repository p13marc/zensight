//! TLS syslog transport (#550, RFC 5425): build a rustls server config from PEM
//! cert/key files (+ optional client-CA for mTLS) and extract the client-cert
//! CN for sender attribution. The accept loop lives in `receiver.rs` (it needs
//! the private ingest context); this module is the pure config + parsing side.

use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;

use crate::config::TlsListenerConfig;

/// The mtime pair (cert, key, client-ca) used to detect rotation for reload.
pub type CertMtimes = (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>);

/// Resolve a path that may use `${ENV}` / `file:` secret indirection (#538).
fn resolve_path(value: &str) -> Result<String> {
    zensight_sensor_core::resolve_secret(value).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Read the mtimes of the configured cert/key/CA files (for rotation detection).
pub fn cert_mtimes(tls: &TlsListenerConfig) -> CertMtimes {
    let m = |p: &str| {
        resolve_path(p)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|md| md.modified().ok())
    };
    (
        m(&tls.cert_file),
        m(&tls.key_file),
        tls.client_ca_file.as_deref().and_then(m),
    )
}

/// Build a rustls [`ServerConfig`] from the listener's cert/key (+ optional mTLS
/// client-CA), enforcing the configured minimum TLS version.
pub fn load_server_config(tls: &TlsListenerConfig) -> Result<Arc<ServerConfig>> {
    let cert_path = resolve_path(&tls.cert_file)?;
    let key_path = resolve_path(&tls.key_file)?;

    let certs = load_certs(&cert_path)?;
    let key = load_key(&key_path)?;

    // Restrict protocol versions per `min_version` (default 1.3).
    let versions: &[&rustls::SupportedProtocolVersion] = match tls.min_version.as_str() {
        "1.2" => &[&rustls::version::TLS13, &rustls::version::TLS12],
        // Default / "1.3": 1.3 only.
        _ => &[&rustls::version::TLS13],
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .context("configure TLS versions")?;

    let config = match &tls.client_ca_file {
        Some(ca) => {
            // mTLS: require + verify client certs against the CA bundle. Build the
            // verifier with our explicit provider — never a process-wide default
            // (which may be uninstalled or ambiguous when multiple providers link).
            let ca_path = resolve_path(ca)?;
            let mut roots = rustls::RootCertStore::empty();
            for cert in load_certs(&ca_path)? {
                roots.add(cert).context("add client CA cert")?;
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .context("build client-cert verifier")?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .context("load server cert/key")?
        }
        None => builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("load server cert/key")?,
    };
    Ok(Arc::new(config))
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path).with_context(|| format!("read cert file {path}"))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut data.as_slice())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parse PEM certs from {path}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {path}");
    }
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path).with_context(|| format!("read key file {path}"))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .with_context(|| format!("parse PEM key from {path}"))?
        .with_context(|| format!("no private key found in {path}"))
}

/// Extract the leaf client certificate's Common Name (mTLS peer identity), if
/// any. Returns `None` when there is no client cert or no CN.
pub fn peer_cn(certs: Option<&[CertificateDer<'_>]>) -> Option<String> {
    let leaf = certs?.first()?;
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref()).ok()?;
    parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed cert/key round-trips through `load_server_config` (server
    /// auth), and mTLS config builds when a client CA is supplied.
    #[test]
    fn builds_server_config_from_generated_certs() {
        let dir = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        let tls = TlsListenerConfig {
            cert_file: cert_path.to_string_lossy().into_owned(),
            key_file: key_path.to_string_lossy().into_owned(),
            client_ca_file: None,
            min_version: "1.3".into(),
        };
        assert!(
            load_server_config(&tls).is_ok(),
            "server-auth config builds"
        );

        // mTLS: reuse the same cert as a CA for the test.
        let ca_path = dir.path().join("ca.crt");
        std::fs::write(&ca_path, cert.cert.pem()).unwrap();
        let mtls = TlsListenerConfig {
            client_ca_file: Some(ca_path.to_string_lossy().into_owned()),
            ..tls
        };
        assert!(load_server_config(&mtls).is_ok(), "mTLS config builds");
    }

    #[test]
    fn peer_cn_none_without_certs() {
        assert_eq!(peer_cn(None), None);
        assert_eq!(peer_cn(Some(&[])), None);
    }

    #[test]
    fn missing_cert_file_errors() {
        let tls = TlsListenerConfig {
            cert_file: "/definitely/not/here.crt".into(),
            key_file: "/definitely/not/here.key".into(),
            client_ca_file: None,
            min_version: "1.3".into(),
        };
        assert!(load_server_config(&tls).is_err());
    }
}
