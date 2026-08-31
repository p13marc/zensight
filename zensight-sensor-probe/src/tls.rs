//! TLS inspection, from a socket or from a file (#820).
//!
//! Deliberately not done through the HTTP client: reading a certificate's
//! expiry, issuer and SANs means *holding* the peer's chain, and an HTTP
//! client's job is to validate it and throw it away. `tokio-rustls` hands the
//! peer chain over; `x509-parser` reads it. The same parser reads a PEM off
//! disk, so a socket handshake and a local file produce **identical
//! documents** — which is the point: one sensor answers "is this certificate
//! good" whether it comes off a socket or off a disk.
//!
//! `inspect_untrusted` completes the handshake even when the chain does not
//! validate, so an expired or self-signed certificate can be *reported* rather
//! than producing a bare connection error. **Reading a certificate is not
//! trusting it**: the verdict is published as `chain_valid: false`, and the
//! bytes are only ever parsed, never acted on.

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use x509_parser::prelude::*;

use zensight_common::probe::{TlsResult, san_matches};

/// A verifier that records what the real one thought and then allows the
/// handshake regardless, so the certificate can be *read*.
///
/// Scoped to this sensor's inspection path and to nothing else: it is
/// constructed per-probe, it never touches a client used for anything but
/// reading a certificate, and its verdict is published rather than discarded.
#[derive(Debug)]
struct RecordingVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    valid: Arc<std::sync::atomic::AtomicBool>,
}

impl ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verdict =
            self.inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp, now);
        self.valid
            .store(verdict.is_ok(), std::sync::atomic::Ordering::Relaxed);
        // Deliberately allow: the caller asked to inspect, and the real
        // verdict is what gets published.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn root_store() -> Arc<rustls::RootCertStore> {
    Arc::new(rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    })
}

/// Handshake with `addr`, presenting `server_name`, and report what the peer
/// showed.
pub async fn inspect_socket(
    addr: &str,
    server_name: &str,
    timeout: Duration,
    inspect_untrusted: bool,
) -> Result<TlsResult, String> {
    let roots = root_store();
    let valid = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cfg = if inspect_untrusted {
        let inner = rustls::client::WebPkiServerVerifier::builder(roots.clone())
            .build()
            .map_err(|e| e.to_string())?;
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(RecordingVerifier {
                inner,
                valid: valid.clone(),
            }))
            .with_no_client_auth()
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    cfg.alpn_protocols.clear();

    let sni = ServerName::try_from(server_name.to_string())
        .map_err(|_| format!("{server_name:?} is not a valid server name"))?;
    let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tls = tokio::time::timeout(timeout, connector.connect(sni, stream))
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| e.to_string())?;

    let (_, conn) = tls.get_ref();
    let protocol = conn.protocol_version().map(|v| format!("{v:?}"));
    let chain = conn
        .peer_certificates()
        .ok_or_else(|| "the peer presented no certificate".to_string())?;
    let leaf = chain
        .first()
        .ok_or_else(|| "the peer presented an empty chain".to_string())?;

    let mut out = parse_der(leaf.as_ref(), server_name)?;
    out.protocol = protocol;
    out.chain_valid = Some(if inspect_untrusted {
        valid.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        // A handshake that completed without the recording verifier means the
        // real one accepted it.
        true
    });
    Ok(out)
}

/// Read a PEM off disk and report the same document a socket would give.
///
/// The one non-network check that belongs in this sensor. It retires the
/// monthly cron that warns when a ZenSight *mesh* certificate is within 60
/// days of expiry, and removes the oddity of a supervision system needing an
/// external timer to watch its own certificates.
pub fn inspect_file(path: &str, expect_name: Option<&str>) -> Result<TlsResult, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    let first = rustls_pemfile::certs(&mut reader)
        .next()
        .ok_or_else(|| format!("{path}: no certificate in the file"))?
        .map_err(|e| format!("{path}: {e}"))?;
    let mut out = parse_der(first.as_ref(), expect_name.unwrap_or(""))?;
    // A file on disk has no chain to validate against a trust store, and
    // claiming `true` would be inventing a verdict nobody produced.
    out.chain_valid = None;
    if expect_name.is_none() {
        out.san_matched = None;
    }
    Ok(out)
}

/// Parse one DER certificate into the published document.
fn parse_der(der: &[u8], name: &str) -> Result<TlsResult, String> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| format!("bad certificate: {e}"))?;
    let not_after = cert.validity().not_after.timestamp();
    let not_before = cert.validity().not_before.timestamp();
    let now = zensight_common::current_timestamp_millis() / 1000;
    let sans: Vec<String> = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|g| match g {
                    GeneralName::DNSName(d) => Some((*d).to_string()),
                    GeneralName::IPAddress(ip) => Some(render_ip(ip)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(TlsResult {
        // Truncating toward zero, so "expires in 23 hours" is 0 days rather
        // than 1 — the pessimistic direction is the correct one here.
        days_to_expiry: Some((not_after - now) / 86_400),
        not_after: Some(not_after),
        not_before: Some(not_before),
        issuer: Some(cert.issuer().to_string()),
        subject: Some(cert.subject().to_string()),
        san_matched: (!name.is_empty()).then(|| sans.iter().any(|s| san_matches(s, name))),
        sans,
        protocol: None,
        chain_valid: None,
    })
}

fn render_ip(raw: &[u8]) -> String {
    match raw.len() {
        4 => std::net::Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]).to_string(),
        16 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(raw);
            std::net::Ipv6Addr::from(o).to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed file must say so rather than producing a document full of
    /// defaults that reads like a valid certificate.
    #[test]
    fn a_file_that_is_not_a_certificate_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nope.pem");
        std::fs::write(&p, b"this is not a certificate\n").unwrap();
        let e = inspect_file(p.to_str().unwrap(), None).unwrap_err();
        assert!(e.contains("no certificate in the file"), "{e}");
    }

    #[test]
    fn a_missing_file_is_an_error_naming_the_path() {
        let e = inspect_file("/nonexistent/cert.pem", None).unwrap_err();
        assert!(e.contains("/nonexistent/cert.pem"), "{e}");
    }

    /// A file has no trust chain, and claiming one validated would be
    /// inventing a verdict nobody produced.
    #[test]
    fn a_file_reports_no_chain_verdict() {
        // Uses the certificate the e2e test generates; skipped when absent.
        let Some(pem) = std::env::var_os("ZENSIGHT_TEST_CERT") else {
            return;
        };
        let r = inspect_file(pem.to_str().unwrap(), None).unwrap();
        assert_eq!(r.chain_valid, None);
        assert_eq!(r.san_matched, None);
    }
}
