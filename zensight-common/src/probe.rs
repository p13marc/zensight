//! Synthetic-probe wire types (#820).
//!
//! Everything else ZenSight measures is *inside*. Nothing checked that the
//! thing works from **outside** — that the site answers, that the certificate
//! is valid, that the name resolves, that the port is reachable from where a
//! user actually is.
//!
//! That gap cost twice on the reference fleet:
//!
//! - **The `/etc/hosts` hairpin, 2026-08-20 → 2026-08-28.** A reboot dropped a
//!   hosts entry, so a guest resolved `git.marcpardo.eu` to the public IP —
//!   which a guest cannot reach, because the edge DNAT matches the external
//!   interface only. cosign and Renovate both broke. **The diagnosis took eight
//!   days** and eventually hinged on noticing that failing runs took 2m16s — a
//!   20 s connect timeout — and that the forge's router log showed zero
//!   requests. A guest-side probe of that URL would have said "timeout, 20 s"
//!   within one interval, on day one. Instead the failures were first
//!   attributed to expired tokens and two issues were filed on that theory.
//! - **TLS.** The only outside-in check the fleet had was a shell script
//!   probing seven vhosts by SNI on a timer.
//!
//! The one non-network check that belongs here is **local certificate files**:
//! read a PEM off disk and publish its `notAfter`. It removes the oddity of a
//! supervision system needing an external cron to watch its own certificates.

use serde::{Deserialize, Serialize};

use schemars::JsonSchema;

/// What a target is checked with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProbeKind {
    Http,
    Tls,
    Dns,
    Tcp,
    Icmp,
    /// A PEM on disk. No network at all.
    CertFile,
}

impl ProbeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeKind::Http => "http",
            ProbeKind::Tls => "tls",
            ProbeKind::Dns => "dns",
            ProbeKind::Tcp => "tcp",
            ProbeKind::Icmp => "icmp",
            ProbeKind::CertFile => "certfile",
        }
    }
}

impl std::fmt::Display for ProbeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The verdict.
///
/// `Timeout` is deliberately **not** folded into `Failed`. The 2026-08-20
/// hairpin's whole signature was a *timeout* rather than a refusal or an
/// error: a connection that hangs for the full deadline means packets are
/// going somewhere that never answers, which is a different diagnosis from
/// "the service said no". Eight days went into rediscovering that distinction
/// by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProbeOutcome {
    Ok,
    Failed,
    Timeout,
}

impl ProbeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeOutcome::Ok => "ok",
            ProbeOutcome::Failed => "failed",
            ProbeOutcome::Timeout => "timeout",
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, ProbeOutcome::Ok)
    }
}

impl std::fmt::Display for ProbeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// What one HTTP check saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct HttpResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_matched: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_matched: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<f64>,
    /// Every URL the request passed through, in order. A redirect chain that
    /// leaves the configured host is a finding, not a detail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// What one TLS handshake saw — from a socket, or from a file on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TlsResult {
    /// Days until `notAfter`. Negative once expired, which is a state worth
    /// being able to express rather than clamping to zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days_to_expiry: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sans: Vec<String>,
    /// Whether the requested name is covered by the certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub san_matched: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_valid: Option<bool>,
}

/// What one DNS lookup saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct DnsResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<String>,
    /// **The resolver that answered**, named. Without it "resolves to the
    /// wrong address" is not expressible — and that sentence is exactly the
    /// 2026-08-20 hairpin, which was a *different answer from a different
    /// resolver on a different host*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_matched: Option<bool>,
}

/// One target's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    /// The operator's name for this target; the device slug and the alert key.
    pub name: String,
    pub kind: ProbeKind,
    /// What was checked, verbatim — the URL, `host:port`, name, or path.
    pub target: String,
    pub outcome: ProbeOutcome,
    /// Total elapsed, milliseconds. **Present even on a timeout**: the
    /// duration is the diagnosis. 2m16s of a CI job was 20 s of connect
    /// timeout repeated, and recognising that took eight days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    /// Why it failed, in the checker's own words. Never invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsResult>,
    /// Where this probe ran from. The same target checked from the edge, from
    /// a guest and from a workstation gives three different and equally true
    /// answers; without the vantage point they are indistinguishable.
    pub vantage: String,
    pub observed_at_ms: i64,
}

impl ProbeResult {
    /// Days to expiry, from a socket handshake or a PEM on disk.
    pub fn days_to_expiry(&self) -> Option<i64> {
        self.tls.as_ref().and_then(|t| t.days_to_expiry)
    }
}

/// Whether `name` is covered by a certificate SAN, honouring a single leading
/// wildcard label.
///
/// `*.example.com` matches `a.example.com` but **not** `a.b.example.com` and
/// not the bare `example.com` — the rule every TLS library implements and
/// every hand-rolled check gets wrong in one of those two directions.
pub fn san_matches(san: &str, name: &str) -> bool {
    let san = san.trim().to_ascii_lowercase();
    let name = name.trim().to_ascii_lowercase();
    if let Some(suffix) = san.strip_prefix("*.") {
        return name.split_once('.').is_some_and(|(_, rest)| rest == suffix);
    }
    san == name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_san_covers_exactly_one_label() {
        assert!(san_matches("*.example.com", "a.example.com"));
        assert!(san_matches("*.example.com", "A.Example.COM"));
        assert!(
            !san_matches("*.example.com", "a.b.example.com"),
            "a wildcard covers one label, not a subtree"
        );
        assert!(
            !san_matches("*.example.com", "example.com"),
            "and not the bare name"
        );
    }

    #[test]
    fn an_exact_san_is_exact() {
        assert!(san_matches("git.marcpardo.eu", "git.marcpardo.eu"));
        assert!(!san_matches("git.marcpardo.eu", "www.marcpardo.eu"));
    }

    /// A timeout is not a failure with a different label: it is the signature
    /// of packets going somewhere that never answers, which is a different
    /// diagnosis from a refusal. Eight days went into rediscovering that.
    #[test]
    fn timeout_is_its_own_outcome() {
        assert_ne!(ProbeOutcome::Timeout, ProbeOutcome::Failed);
        assert!(!ProbeOutcome::Timeout.is_ok());
        assert_eq!(ProbeOutcome::Timeout.as_str(), "timeout");
    }

    /// An expired certificate has a negative time to expiry. Clamping it to
    /// zero would make "expired an hour ago" and "expires in a month" look
    /// equally survivable on a graph.
    #[test]
    fn expiry_can_be_negative() {
        let r = ProbeResult {
            name: "mesh".into(),
            kind: ProbeKind::CertFile,
            target: "/etc/zensight/tls/cert.pem".into(),
            outcome: ProbeOutcome::Ok,
            duration_ms: Some(1.0),
            error: None,
            http: None,
            tls: Some(TlsResult {
                days_to_expiry: Some(-3),
                ..Default::default()
            }),
            dns: None,
            vantage: "vm-dev".into(),
            observed_at_ms: 0,
        };
        assert_eq!(r.days_to_expiry(), Some(-3));
    }
}
