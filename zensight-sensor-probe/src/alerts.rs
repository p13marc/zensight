//! The probe assertions (#820).
//!
//! Pure, so the rule table is testable against results without a network.
//! Every alert carries the **vantage point**: the same target checked from the
//! edge, from a guest and from a workstation gives three different and equally
//! true answers, and an alert that does not say where it looked from is not
//! actionable.

use std::collections::HashMap;

use zensight_common::probe::{ProbeOutcome, ProbeResult};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

use crate::config::ProbeAlertsConfig;

pub const RULE_DOWN: &str = "probe-down";
pub const RULE_TIMEOUT: &str = "probe-timeout";
pub const RULE_OFFHOST_REDIRECT: &str = "probe-redirect-off-host";
pub const RULE_CERT_EXPIRING: &str = "probe-certificate-expiring";
pub const RULE_CHAIN_INVALID: &str = "probe-certificate-chain-invalid";
pub const RULE_SAN_MISMATCH: &str = "probe-certificate-name-mismatch";
pub const RULE_DNS_UNEXPECTED: &str = "probe-dns-unexpected-answer";

pub const ALL_RULES: &[&str] = &[
    RULE_DOWN,
    RULE_TIMEOUT,
    RULE_OFFHOST_REDIRECT,
    RULE_CERT_EXPIRING,
    RULE_CHAIN_INVALID,
    RULE_SAN_MISMATCH,
    RULE_DNS_UNEXPECTED,
];

fn alert(
    source: &str,
    r: &ProbeResult,
    rule: &str,
    severity: AlertSeverity,
    summary: String,
    extra: &[(&str, String)],
) -> Alert {
    let mut a = Alert::new(
        source,
        Protocol::Probe,
        AlertKind::Expectation,
        rule,
        severity,
        summary,
    );
    let mut labels = HashMap::new();
    // The operator's own handle for this check. It is the key chunk on the
    // telemetry side, but an alert key is a digest, so without this label the
    // alert named only the URL — and since #883 moved `source` to the vantage
    // point, nothing on the alert said *which configured target* it was about.
    labels.insert("probe".to_string(), r.name.clone());
    labels.insert("target".to_string(), r.target.clone());
    labels.insert("kind".to_string(), r.kind.to_string());
    // Half the answer. Two hosts probing the same URL and disagreeing is not a
    // contradiction — it is the finding.
    labels.insert("vantage".to_string(), r.vantage.clone());
    // NOT `duration_ms`. Labels are the alert's identity (`alert_key` hashes
    // every non-`host.*` label), and a fresh wall-clock measurement on every
    // check minted a new key every sweep — so no probe alert ever stayed on
    // one key long enough for the `for:` debounce to elapse, and a target
    // that was down for a week never paged anyone. The measurement is a
    // telemetry point (`{target}/duration_ms`) and rides the summary where
    // it is a diagnosis.
    for (k, v) in extra {
        labels.insert((*k).to_string(), v.clone());
    }
    a.labels = labels;
    a
}

/// Grade one sweep. `source` is the reporting host — the vantage point — and
/// is the `source` of every alert, as of #883: a probe result is an
/// observation made from somewhere, and two hosts probing the same target
/// must not collide on one identity. The target rides in the labels.
pub fn grade(cfg: &ProbeAlertsConfig, source: &str, results: &[ProbeResult]) -> Vec<Alert> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }
    for r in results {
        match r.outcome {
            // A timeout gets its own rule AND suppresses the generic
            // down alert, so an operator gets one page with the right
            // diagnosis rather than two with a vaguer one on top.
            ProbeOutcome::Timeout if cfg.timeout => out.push(alert(
                source,
                r,
                RULE_TIMEOUT,
                AlertSeverity::Critical,
                format!(
                    "{} timed out after {:.0} ms — packets are going somewhere that never \
                     answers, which is not the same as being refused",
                    r.target,
                    r.duration_ms.unwrap_or(0.0)
                ),
                &[("error", r.error.clone().unwrap_or_default())],
            )),
            ProbeOutcome::Timeout | ProbeOutcome::Failed if cfg.down => out.push(alert(
                source,
                r,
                RULE_DOWN,
                AlertSeverity::Critical,
                format!(
                    "{} failed: {}",
                    r.target,
                    r.error.as_deref().unwrap_or("no detail")
                ),
                &[("error", r.error.clone().unwrap_or_default())],
            )),
            _ => {}
        }

        if cfg.offhost_redirect
            && let Some(h) = &r.http
            && r.error
                .as_deref()
                .is_some_and(|e| e.contains("redirected off the configured host"))
        {
            out.push(alert(
                source,
                r,
                RULE_OFFHOST_REDIRECT,
                AlertSeverity::Warning,
                format!(
                    "{} redirected off its configured host to {}",
                    r.target,
                    h.redirects.last().cloned().unwrap_or_default()
                ),
                &[("final_url", h.redirects.last().cloned().unwrap_or_default())],
            ));
        }

        let Some(tls) = &r.tls else { continue };

        if let Some(days) = tls.days_to_expiry
            && cfg.expiry_warn_days > 0
            && days <= cfg.expiry_warn_days
        {
            out.push(alert(
                source,
                r,
                RULE_CERT_EXPIRING,
                if days <= cfg.expiry_critical_days {
                    AlertSeverity::Critical
                } else {
                    AlertSeverity::Warning
                },
                if days < 0 {
                    format!("{}'s certificate EXPIRED {} days ago", r.target, days.abs())
                } else {
                    format!("{}'s certificate expires in {days} days", r.target)
                },
                &[
                    ("days_to_expiry", days.to_string()),
                    ("issuer", tls.issuer.clone().unwrap_or_default()),
                ],
            ));
        }

        // `None` means nothing validated a chain — a PEM on disk has no chain
        // to check — and must not read as "invalid".
        if cfg.chain_invalid && tls.chain_valid == Some(false) {
            out.push(alert(
                source,
                r,
                RULE_CHAIN_INVALID,
                AlertSeverity::Critical,
                format!(
                    "{}'s certificate chain did not validate (issuer: {})",
                    r.target,
                    tls.issuer.as_deref().unwrap_or("unknown")
                ),
                &[("issuer", tls.issuer.clone().unwrap_or_default())],
            ));
        }

        // Likewise `None`: no name was asked about, so no verdict exists.
        if cfg.san_mismatch && tls.san_matched == Some(false) {
            out.push(alert(
                source,
                r,
                RULE_SAN_MISMATCH,
                AlertSeverity::Critical,
                format!(
                    "{}'s certificate does not cover the name asked for (SANs: {})",
                    r.target,
                    tls.sans.join(", ")
                ),
                &[("sans", tls.sans.join(","))],
            ));
        }
    }

    for r in results {
        if cfg.dns_unexpected
            && let Some(d) = &r.dns
            && d.expected_matched == Some(false)
        {
            out.push(alert(
                source,
                r,
                RULE_DNS_UNEXPECTED,
                AlertSeverity::Critical,
                format!(
                    "{} resolved to {:?} via {} — not what this vantage point expects",
                    r.target,
                    d.answers,
                    d.resolver.as_deref().unwrap_or("an unnamed resolver")
                ),
                &[
                    ("answers", d.answers.join(",")),
                    ("resolver", d.resolver.clone().unwrap_or_default()),
                ],
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::probe::{DnsResult, HttpResult, ProbeKind, TlsResult};

    /// The reporting host: every alert is filed under it, never under the
    /// target being probed (#883).
    const HOST: &str = "workstation01";

    fn base(kind: ProbeKind, outcome: ProbeOutcome) -> ProbeResult {
        ProbeResult {
            name: "forge".into(),
            kind,
            target: "https://git.marcpardo.eu/".into(),
            outcome,
            duration_ms: Some(20_000.0),
            error: None,
            http: None,
            tls: None,
            dns: None,
            vantage: "vm-apps".into(),
            observed_at_ms: 0,
        }
    }

    fn rules(a: &[Alert]) -> Vec<&str> {
        let mut r: Vec<&str> = a.iter().map(|x| x.rule.as_str()).collect();
        r.sort_unstable();
        r
    }

    /// The 2026-08-20 hairpin, as this sensor would have reported it on day
    /// one: a timeout, with its duration, from a named vantage point.
    #[test]
    fn the_hairpin_reports_as_a_timeout_with_its_duration_and_vantage() {
        let mut r = base(ProbeKind::Http, ProbeOutcome::Timeout);
        r.error = Some("operation timed out".into());
        let a = grade(&ProbeAlertsConfig::default(), HOST, &[r]);
        assert_eq!(rules(&a), vec![RULE_TIMEOUT], "and NOT also probe-down");
        assert_eq!(a[0].labels["duration_ms"], "20000");
        assert_eq!(a[0].labels["vantage"], "vm-apps");
        assert!(
            a[0].summary.contains("not the same as being refused"),
            "{}",
            a[0].summary
        );
    }

    #[test]
    fn a_plain_failure_is_the_down_rule() {
        let mut r = base(ProbeKind::Http, ProbeOutcome::Failed);
        r.error = Some("connection refused".into());
        assert_eq!(
            rules(&grade(&ProbeAlertsConfig::default(), HOST, &[r])),
            vec![RULE_DOWN]
        );
    }

    #[test]
    fn a_healthy_target_fires_nothing() {
        assert!(
            grade(
                &ProbeAlertsConfig::default(),
                HOST,
                &[base(ProbeKind::Http, ProbeOutcome::Ok)]
            )
            .is_empty()
        );
    }

    #[test]
    fn certificate_expiry_escalates_and_can_be_negative() {
        for (days, want) in [
            (60, None),
            (20, Some(AlertSeverity::Warning)),
            (3, Some(AlertSeverity::Critical)),
            (-2, Some(AlertSeverity::Critical)),
        ] {
            let mut r = base(ProbeKind::Tls, ProbeOutcome::Ok);
            r.tls = Some(TlsResult {
                days_to_expiry: Some(days),
                ..Default::default()
            });
            let a = grade(&ProbeAlertsConfig::default(), HOST, &[r]);
            match want {
                None => assert!(a.is_empty(), "{days} days should not fire"),
                Some(sev) => {
                    assert_eq!(rules(&a), vec![RULE_CERT_EXPIRING], "{days}");
                    assert_eq!(a[0].severity, sev, "{days}");
                    if days < 0 {
                        assert!(a[0].summary.contains("EXPIRED"), "{}", a[0].summary);
                    }
                }
            }
        }
    }

    /// A PEM on disk has no chain to validate against a trust store, and no
    /// name was asked about. `None` in either field must not fire.
    #[test]
    fn an_absent_verdict_is_not_a_negative_one() {
        let mut r = base(ProbeKind::CertFile, ProbeOutcome::Ok);
        r.tls = Some(TlsResult {
            days_to_expiry: Some(400),
            chain_valid: None,
            san_matched: None,
            ..Default::default()
        });
        assert!(grade(&ProbeAlertsConfig::default(), HOST, &[r]).is_empty());
    }

    #[test]
    fn an_invalid_chain_and_a_name_mismatch_are_separate_findings() {
        let mut r = base(ProbeKind::Tls, ProbeOutcome::Ok);
        r.tls = Some(TlsResult {
            days_to_expiry: Some(400),
            chain_valid: Some(false),
            san_matched: Some(false),
            sans: vec!["other.example".into()],
            ..Default::default()
        });
        assert_eq!(
            rules(&grade(&ProbeAlertsConfig::default(), HOST, &[r])),
            vec![RULE_CHAIN_INVALID, RULE_SAN_MISMATCH]
        );
    }

    /// The other half of the hairpin: the name resolved, just to the wrong
    /// place — and the alert has to name the resolver, or it says nothing.
    #[test]
    fn a_wrong_dns_answer_names_the_resolver_that_gave_it() {
        let mut r = base(ProbeKind::Dns, ProbeOutcome::Failed);
        r.dns = Some(DnsResult {
            answers: vec!["203.0.113.7".into()],
            resolver: Some("127.0.0.53".into()),
            expected_matched: Some(false),
        });
        let a = grade(&ProbeAlertsConfig::default(), HOST, &[r]);
        assert!(rules(&a).contains(&RULE_DNS_UNEXPECTED));
        let dns = a.iter().find(|x| x.rule == RULE_DNS_UNEXPECTED).unwrap();
        assert_eq!(dns.labels["resolver"], "127.0.0.53");
        assert_eq!(dns.labels["answers"], "203.0.113.7");
    }

    #[test]
    fn an_off_host_redirect_is_its_own_finding() {
        let mut r = base(ProbeKind::Http, ProbeOutcome::Failed);
        r.error = Some("redirected off the configured host to https://elsewhere.example/".into());
        r.http = Some(HttpResult {
            redirects: vec!["https://elsewhere.example/".into()],
            ..Default::default()
        });
        let a = grade(&ProbeAlertsConfig::default(), HOST, &[r]);
        assert!(rules(&a).contains(&RULE_OFFHOST_REDIRECT));
    }

    #[test]
    fn every_emitted_rule_is_reconciled() {
        let mut timeout = base(ProbeKind::Http, ProbeOutcome::Timeout);
        timeout.name = "a".into();
        let mut down = base(ProbeKind::Http, ProbeOutcome::Failed);
        down.name = "b".into();
        let mut redirect = base(ProbeKind::Http, ProbeOutcome::Failed);
        redirect.name = "c".into();
        redirect.error = Some("redirected off the configured host to https://x/".into());
        redirect.http = Some(HttpResult::default());
        let mut cert = base(ProbeKind::Tls, ProbeOutcome::Ok);
        cert.name = "d".into();
        cert.tls = Some(TlsResult {
            days_to_expiry: Some(1),
            chain_valid: Some(false),
            san_matched: Some(false),
            ..Default::default()
        });
        let mut dns = base(ProbeKind::Dns, ProbeOutcome::Ok);
        dns.name = "e".into();
        dns.dns = Some(DnsResult {
            answers: vec!["1.2.3.4".into()],
            resolver: Some("system".into()),
            expected_matched: Some(false),
        });

        let fired: std::collections::HashSet<String> = grade(
            &ProbeAlertsConfig::default(),
            HOST,
            &[timeout, down, redirect, cert, dns],
        )
        .iter()
        .map(|a| a.rule.clone())
        .collect();
        assert_eq!(fired.len(), ALL_RULES.len(), "fired: {fired:?}");
        for r in &fired {
            assert!(ALL_RULES.contains(&r.as_str()), "{r} is not reconciled");
        }
    }
}
