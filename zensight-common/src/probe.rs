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
    /// A **burst** of probes in one interval, producing delay variation and
    /// loss (#958). A single-shot check per interval cannot produce a jitter
    /// figure at all — one sample has no variation — which is why this is a
    /// kind of its own rather than a flag on `tcp`/`icmp`.
    Burst,
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
            ProbeKind::Burst => "burst",
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

/// A burst's delay-variation and loss figures (#958).
///
/// Smokeping-shaped: `count` probes spaced `spacing_ms` apart within one
/// interval, reduced to the numbers a link is judged by.
///
/// **Every RTT field is optional and absent means "not measured".** A burst in
/// which every probe was lost publishes `loss_pct: 100` and *no* RTT or jitter
/// — not zeros. A zero would be indistinguishable from a perfect link, which is
/// the exact opposite of what happened, and a consumer averaging it would
/// silently improve the fleet's numbers every time a link died.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BurstResult {
    /// Probes attempted.
    pub sent: u32,
    /// Probes that answered.
    pub received: u32,
    /// Loss as a percentage of `sent`. Always present — a total loss is a
    /// measurement, unlike the RTT fields it suppresses.
    pub loss_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_min_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_avg_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_max_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_p95_ms: Option<f64>,
    /// Mean absolute inter-packet delay variation (RFC 3393 IPDV, the
    /// smokeping/RFC 1889 definition) over **consecutive successful** probes.
    ///
    /// Needs at least two successes to exist, so a burst with one survivor
    /// publishes loss and RTTs but no jitter. Computing it over
    /// non-consecutive probes would measure the gaps the losses left, not the
    /// link's delay variation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_ms: Option<f64>,
    /// Which transport produced these numbers: `"tcp"` or `"icmp"`. They are
    /// not comparable — a TCP connect RTT includes the peer's accept path —
    /// so a consumer charting both needs to know which it has.
    pub transport: String,
}

impl BurstResult {
    /// Reduce a burst's per-probe outcomes to the published figures.
    ///
    /// `samples` is one entry per probe **in send order**, `None` for a probe
    /// that did not answer. Order matters: jitter is computed only across
    /// consecutive successes, and reordering the input would measure something
    /// else.
    pub fn reduce(samples: &[Option<f64>], transport: &str) -> BurstResult {
        let sent = samples.len() as u32;
        let ok: Vec<f64> = samples.iter().flatten().copied().collect();
        let received = ok.len() as u32;
        let loss_pct = if sent == 0 {
            0.0
        } else {
            ((sent - received) as f64 / sent as f64) * 100.0
        };
        if ok.is_empty() {
            return BurstResult {
                sent,
                received,
                loss_pct,
                rtt_min_ms: None,
                rtt_avg_ms: None,
                rtt_max_ms: None,
                rtt_p95_ms: None,
                jitter_ms: None,
                transport: transport.to_string(),
            };
        }
        let mut sorted = ok.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Nearest-rank p95: with ten samples the honest answer is the largest,
        // and an interpolating percentile would invent a value between two
        // measurements that a link never exhibited.
        let idx = (((sorted.len() as f64) * 0.95).ceil() as usize).max(1) - 1;
        let p95 = sorted[idx.min(sorted.len() - 1)];

        // IPDV over CONSECUTIVE successes only. `samples` is in send order, so
        // a lost probe breaks the chain rather than joining the probes either
        // side of it — otherwise a burst that lost its middle would report the
        // gap the loss left as delay variation.
        let mut diffs = Vec::new();
        let mut prev: Option<f64> = None;
        for s in samples {
            match s {
                Some(v) => {
                    if let Some(p) = prev {
                        diffs.push((v - p).abs());
                    }
                    prev = Some(*v);
                }
                None => prev = None,
            }
        }
        let jitter_ms = (!diffs.is_empty()).then(|| diffs.iter().sum::<f64>() / diffs.len() as f64);

        BurstResult {
            sent,
            received,
            loss_pct,
            rtt_min_ms: Some(sorted[0]),
            rtt_avg_ms: Some(ok.iter().sum::<f64>() / ok.len() as f64),
            rtt_max_ms: Some(sorted[sorted.len() - 1]),
            rtt_p95_ms: Some(p95),
            jitter_ms,
            transport: transport.to_string(),
        }
    }
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
    /// Delay variation and loss, for a `burst` check (#958).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstResult>,
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

    /// The figures a burst reduces to, on a burst that lost nothing.
    #[test]
    fn a_clean_burst_reduces_to_its_measurements() {
        let b = BurstResult::reduce(&[Some(10.0), Some(12.0), Some(11.0), Some(30.0)], "tcp");
        assert_eq!((b.sent, b.received), (4, 4));
        assert_eq!(b.loss_pct, 0.0);
        assert_eq!(b.rtt_min_ms, Some(10.0));
        assert_eq!(b.rtt_max_ms, Some(30.0));
        assert_eq!(b.rtt_avg_ms, Some(15.75));
        // Nearest-rank p95 over four samples is the largest: an interpolating
        // percentile would invent a value the link never exhibited.
        assert_eq!(b.rtt_p95_ms, Some(30.0));
        // IPDV: |12-10| + |11-12| + |30-11| = 2 + 1 + 19, over 3 gaps.
        assert_eq!(b.jitter_ms, Some((2.0 + 1.0 + 19.0) / 3.0));
        assert_eq!(b.transport, "tcp");
    }

    /// A total loss publishes loss and **no RTT fields at all**.
    #[test]
    fn a_total_loss_publishes_no_rtt_not_zero() {
        let b = BurstResult::reduce(&[None, None, None], "icmp");
        assert_eq!((b.sent, b.received), (3, 0));
        assert_eq!(b.loss_pct, 100.0);
        // Zeros here would be indistinguishable from a perfect link — the
        // exact opposite of what happened — and a consumer averaging them
        // would silently improve the fleet's numbers every time a link died.
        assert!(b.rtt_min_ms.is_none());
        assert!(b.rtt_avg_ms.is_none());
        assert!(b.rtt_max_ms.is_none());
        assert!(b.rtt_p95_ms.is_none());
        assert!(b.jitter_ms.is_none());
        // And they are genuinely absent from the wire, not null.
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("rtt_"), "{json}");
        assert!(!json.contains("jitter"), "{json}");
        assert!(json.contains("\"loss_pct\":100"), "{json}");
    }

    /// One survivor gives RTTs but no jitter: variation needs two points.
    #[test]
    fn a_single_survivor_has_rtts_but_no_jitter() {
        let b = BurstResult::reduce(&[None, Some(7.5), None], "tcp");
        assert_eq!(b.received, 1);
        assert_eq!(b.rtt_min_ms, Some(7.5));
        assert_eq!(b.rtt_avg_ms, Some(7.5));
        assert!(
            b.jitter_ms.is_none(),
            "delay variation across one sample is not a number"
        );
    }

    /// Jitter spans only **consecutive** successes.
    #[test]
    fn a_loss_breaks_the_jitter_chain_rather_than_bridging_it() {
        // 10, lost, 100. Bridging would report 90 ms of "delay variation"
        // that is really the gap the loss left; the honest answer is that no
        // consecutive pair was measured, so there is no jitter figure.
        let b = BurstResult::reduce(&[Some(10.0), None, Some(100.0)], "icmp");
        assert_eq!(b.received, 2);
        assert!(
            b.jitter_ms.is_none(),
            "no two consecutive probes both answered: {:?}",
            b.jitter_ms
        );
        // But a pair that IS consecutive still counts, even beside a loss.
        let b = BurstResult::reduce(&[Some(10.0), Some(14.0), None, Some(100.0)], "icmp");
        assert_eq!(b.jitter_ms, Some(4.0));
    }

    /// Order is meaningful: jitter is not a property of the multiset.
    #[test]
    fn reordering_the_samples_changes_the_jitter() {
        let ascending = BurstResult::reduce(&[Some(10.0), Some(20.0), Some(30.0)], "tcp");
        let jagged = BurstResult::reduce(&[Some(10.0), Some(30.0), Some(20.0)], "tcp");
        // Same samples, same min/avg/max — different delay variation.
        assert_eq!(ascending.rtt_avg_ms, jagged.rtt_avg_ms);
        assert_eq!(ascending.jitter_ms, Some(10.0));
        assert_eq!(jagged.jitter_ms, Some(15.0));
    }

    #[test]
    fn loss_is_a_percentage_of_what_was_sent() {
        let b = BurstResult::reduce(&[Some(1.0), None, None, None], "tcp");
        assert_eq!(b.loss_pct, 75.0);
        assert_eq!(BurstResult::reduce(&[], "tcp").loss_pct, 0.0);
    }
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
            burst: None,
            vantage: "vm-dev".into(),
            observed_at_ms: 0,
        };
        assert_eq!(r.days_to_expiry(), Some(-3));
    }
}
