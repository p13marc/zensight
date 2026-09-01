//! The checks (#820).
//!
//! One function per kind, each returning a [`ProbeResult`]. They share three
//! rules:
//!
//! 1. **A timeout is its own outcome.** Not a failure with different text. A
//!    connection that hangs for the full deadline means packets are going
//!    somewhere that never answers, which is a different diagnosis from "the
//!    service said no" — and the 2026-08-20 hairpin was found, eight days
//!    late, by a human noticing exactly that distinction in a CI job's
//!    duration.
//! 2. **The duration is published even when the check fails.** It is often the
//!    whole diagnosis.
//! 3. **The error text is the checker's own.** Never a friendlier
//!    reconstruction: "connection timed out after 20s" is what an operator
//!    needs to recognise, and a paraphrase is a paraphrase.

use std::time::{Duration, Instant};

use zensight_common::probe::{DnsResult, HttpResult, ProbeKind, ProbeOutcome, ProbeResult};

use crate::config::Target;

/// Build the shell of a result, so every path fills the same fields.
fn result(t: &Target, vantage: &str, outcome: ProbeOutcome, started: Instant) -> ProbeResult {
    ProbeResult {
        name: t.name.clone(),
        kind: t.kind,
        target: t.target.clone(),
        outcome,
        duration_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
        error: None,
        http: None,
        tls: None,
        dns: None,
        vantage: vantage.to_string(),
        observed_at_ms: zensight_common::current_timestamp_millis(),
    }
}

/// Run one check.
pub async fn run(
    t: &Target,
    vantage: &str,
    timeout: Duration,
    client: &reqwest::Client,
) -> ProbeResult {
    match t.kind {
        ProbeKind::Http => http(t, vantage, timeout, client).await,
        ProbeKind::Tls => tls(t, vantage, timeout).await,
        ProbeKind::Tcp => tcp(t, vantage, timeout).await,
        ProbeKind::Dns => dns(t, vantage, timeout).await,
        ProbeKind::CertFile => certfile(t, vantage),
        ProbeKind::Icmp => icmp(t, vantage, timeout).await,
    }
}

/// How much of a response body is read when `expect_body` asks for a
/// substring. A probe is a client that pokes operator-supplied URLs; one
/// pointed at a log endpoint or an artifact must not buffer the lot inside a
/// `MemoryMax=64M` unit. A needle that only appears past this point is
/// reported as "not present" with a truncation note, which is a config
/// problem to surface, not a body to keep reading.
const MAX_BODY_BYTES: usize = 256 * 1024;

async fn http(
    t: &Target,
    vantage: &str,
    timeout: Duration,
    client: &reqwest::Client,
) -> ProbeResult {
    let started = Instant::now();
    let method = t.method.as_deref().unwrap_or("GET");
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        let mut r = result(t, vantage, ProbeOutcome::Failed, started);
        r.error = Some(format!("{:?} is not an HTTP method", t.method));
        return r;
    };
    // Per request, not per client: the shared client carries the GLOBAL
    // timeout, and a target's own `timeout_secs` — documented, validated
    // against its interval, and computed by the poller — reached every kind
    // but this one for a while.
    let mut req = client.request(method, &t.target).timeout(timeout);
    for (k, v) in &t.headers {
        req = req.header(k, v);
    }

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let timed_out = e.is_timeout();
            let mut r = result(
                t,
                vantage,
                if timed_out {
                    ProbeOutcome::Timeout
                } else {
                    ProbeOutcome::Failed
                },
                started,
            );
            // The client's own words. A reconstruction would lose exactly the
            // detail that mattered on 2026-08-20.
            r.error = Some(e.to_string());
            return r;
        }
    };
    let ttfb = started.elapsed().as_secs_f64() * 1000.0;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();

    let status_matched = if t.expect_status.is_empty() {
        (200..300).contains(&status)
    } else {
        t.expect_status.contains(&status)
    };
    // The body is read only when something will look at it, and then only
    // up to `MAX_BODY_BYTES`. A plain up/down check used to buffer the whole
    // response — uncapped, on data from the network — for nothing.
    // `bytes` is the server's declared length when the body is not read
    // (the common case), and the bytes actually read when it is — which is
    // the smaller of the body and the cap.
    let mut bytes = resp.content_length();
    let mut body_truncated = false;
    let body_matched = match &t.expect_body {
        None => None,
        Some(needle) => {
            let (body, truncated) = read_body_capped(resp, MAX_BODY_BYTES).await;
            body_truncated = truncated;
            if !truncated {
                bytes = Some(body.len() as u64);
            }
            Some(body.contains(needle.as_str()))
        }
    };

    // A redirect chain that leaves the configured host means the probe is
    // checking something other than what it was asked about.
    let mut redirects = Vec::new();
    if final_url != t.target {
        redirects.push(final_url.clone());
    }
    let off_host = match (t.host(), reqwest::Url::parse(&final_url).ok()) {
        (Some(want), Some(url)) => url.host_str().is_some_and(|h| h != want),
        _ => false,
    };

    let ok =
        status_matched && body_matched.unwrap_or(true) && (t.allow_offhost_redirect || !off_host);

    let mut r = result(
        t,
        vantage,
        if ok {
            ProbeOutcome::Ok
        } else {
            ProbeOutcome::Failed
        },
        started,
    );
    if !ok && off_host && !t.allow_offhost_redirect {
        r.error = Some(format!(
            "redirected off the configured host to {final_url} — set \
             allow_offhost_redirect if that is expected"
        ));
    } else if !status_matched {
        r.error = Some(format!("unexpected status {status}"));
    } else if body_matched == Some(false) {
        r.error = Some(if body_truncated {
            format!(
                "the expected body text was not present in the first {} KiB (the body \
                 was larger and was not read further)",
                MAX_BODY_BYTES / 1024
            )
        } else {
            "the expected body text was not present".to_string()
        });
    }
    r.http = Some(HttpResult {
        status: Some(status),
        status_matched: Some(status_matched),
        body_matched,
        ttfb_ms: Some(ttfb),
        redirects,
        bytes,
    });
    r
}

/// Read at most `cap` bytes of a response body, lossily as UTF-8. Returns
/// the text and whether the body went on past the cap.
async fn read_body_capped(mut resp: reqwest::Response, cap: usize) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = cap.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

async fn tls(t: &Target, vantage: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let name = t.host().unwrap_or_default();
    match crate::tls::inspect_socket(&t.target, &name, timeout, t.inspect_untrusted).await {
        Ok(tls) => {
            // The handshake succeeded, so the check ran. Whether the chain
            // validated, whether the name matches and how long the certificate
            // has left are separate published facts with separate rules — a
            // single boolean here would collapse three different problems.
            let mut r = result(t, vantage, ProbeOutcome::Ok, started);
            r.tls = Some(tls);
            r
        }
        Err(e) => {
            let timed_out = e.contains("timed out");
            let mut r = result(
                t,
                vantage,
                if timed_out {
                    ProbeOutcome::Timeout
                } else {
                    ProbeOutcome::Failed
                },
                started,
            );
            r.error = Some(e);
            r
        }
    }
}

async fn tcp(t: &Target, vantage: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&t.target)).await {
        Ok(Ok(_)) => result(t, vantage, ProbeOutcome::Ok, started),
        Ok(Err(e)) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            r.error = Some(e.to_string());
            r
        }
        Err(_) => {
            let mut r = result(t, vantage, ProbeOutcome::Timeout, started);
            r.error = Some(format!(
                "connect timed out after {}s",
                timeout.as_secs_f64()
            ));
            r
        }
    }
}

async fn dns(t: &Target, vantage: &str, timeout: Duration) -> ProbeResult {
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};

    let started = Instant::now();
    // Whichever resolver answers, it is NAMED in the result. That is the
    // check: on 2026-08-20 one host's resolver gave a different answer from
    // every other host's, and "resolves to the wrong address" was not
    // expressible anywhere in the fleet.
    let (resolver, named) = match &t.resolver {
        Some(spec) => {
            let Ok(addr) = spec
                .parse::<std::net::SocketAddr>()
                .or_else(|_| format!("{spec}:53").parse::<std::net::SocketAddr>())
            else {
                let mut r = result(t, vantage, ProbeOutcome::Failed, started);
                r.error = Some(format!("{spec:?} is not a resolver address"));
                return r;
            };
            // One explicitly-named server, UDP, on the port the operator
            // gave. `trust_negative_responses: true` because this resolver IS
            // the subject of the check: an NXDOMAIN from it is the answer, not
            // a reason to ask somebody else and report their opinion instead.
            let mut conn = ConnectionConfig::udp();
            conn.port = addr.port();
            let server = NameServerConfig::new(addr.ip(), true, vec![conn]);
            (
                hickory_resolver::Resolver::builder_with_config(
                    ResolverConfig::from_parts(None, vec![], vec![server]),
                    hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
                )
                .build()
                .map_err(|e| format!("{spec}: {e}")),
                spec.clone(),
            )
        }
        None => match hickory_resolver::Resolver::builder_tokio() {
            Ok(b) => (
                b.build().map_err(|e| format!("system resolver: {e}")),
                "system".to_string(),
            ),
            Err(e) => {
                let mut r = result(t, vantage, ProbeOutcome::Failed, started);
                r.error = Some(format!("no system resolver: {e}"));
                return r;
            }
        },
    };
    let resolver = match resolver {
        Ok(r) => r,
        Err(e) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            r.error = Some(e);
            return r;
        }
    };

    match tokio::time::timeout(timeout, resolver.lookup_ip(t.target.as_str())).await {
        Ok(Ok(answer)) => {
            let answers: Vec<String> = answer.iter().map(|ip| ip.to_string()).collect();
            let expected_matched = (!t.expect_addrs.is_empty())
                .then(|| t.expect_addrs.iter().all(|e| answers.contains(e)));
            let ok = !answers.is_empty() && expected_matched.unwrap_or(true);
            let mut r = result(
                t,
                vantage,
                if ok {
                    ProbeOutcome::Ok
                } else {
                    ProbeOutcome::Failed
                },
                started,
            );
            if expected_matched == Some(false) {
                r.error = Some(format!(
                    "{named} answered {answers:?}, which does not contain {:?}",
                    t.expect_addrs
                ));
            }
            r.dns = Some(DnsResult {
                answers,
                resolver: Some(named),
                expected_matched,
            });
            r
        }
        Ok(Err(e)) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            r.error = Some(e.to_string());
            r.dns = Some(DnsResult {
                answers: vec![],
                resolver: Some(named),
                expected_matched: None,
            });
            r
        }
        Err(_) => {
            let mut r = result(t, vantage, ProbeOutcome::Timeout, started);
            r.error = Some(format!("{named} did not answer within {timeout:?}"));
            r.dns = Some(DnsResult {
                answers: vec![],
                resolver: Some(named),
                expected_matched: None,
            });
            r
        }
    }
}

fn certfile(t: &Target, vantage: &str) -> ProbeResult {
    let started = Instant::now();
    match crate::tls::inspect_file(&t.target, t.server_name.as_deref()) {
        Ok(tls) => {
            let mut r = result(t, vantage, ProbeOutcome::Ok, started);
            r.tls = Some(tls);
            r
        }
        Err(e) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            r.error = Some(e);
            r
        }
    }
}

#[cfg(feature = "icmp")]
async fn icmp(t: &Target, vantage: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let addr: std::net::IpAddr = match t.target.parse() {
        Ok(a) => a,
        Err(_) => match tokio::net::lookup_host((t.target.as_str(), 0)).await {
            Ok(mut it) => match it.next() {
                Some(sa) => sa.ip(),
                None => {
                    let mut r = result(t, vantage, ProbeOutcome::Failed, started);
                    r.error = Some(format!("{} did not resolve", t.target));
                    return r;
                }
            },
            Err(e) => {
                let mut r = result(t, vantage, ProbeOutcome::Failed, started);
                r.error = Some(e.to_string());
                return r;
            }
        },
    };
    let client = match surge_ping::Client::new(&surge_ping::Config::default()) {
        Ok(c) => c,
        Err(e) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            // The commonest cause by far, and worth naming rather than letting
            // an operator read "permission denied" and guess.
            r.error = Some(format!(
                "{e} (an ICMP probe needs CAP_NET_RAW, or \
                 net.ipv4.ping_group_range covering this process's gid)"
            ));
            return r;
        }
    };
    let mut pinger = client.pinger(addr, surge_ping::PingIdentifier(1)).await;
    pinger.timeout(timeout);
    match pinger.ping(surge_ping::PingSequence(0), &[0u8; 32]).await {
        Ok(_) => result(t, vantage, ProbeOutcome::Ok, started),
        Err(surge_ping::SurgeError::Timeout { .. }) => {
            let mut r = result(t, vantage, ProbeOutcome::Timeout, started);
            r.error = Some(format!("no reply within {timeout:?}"));
            r
        }
        Err(e) => {
            let mut r = result(t, vantage, ProbeOutcome::Failed, started);
            r.error = Some(e.to_string());
            r
        }
    }
}

/// Without the feature the check cannot run, and it says so rather than
/// reporting a failure that looks like the target being down. `validate()`
/// refuses such a target at startup, so this is only reachable if that gate is
/// ever loosened.
#[cfg(not(feature = "icmp"))]
async fn icmp(t: &Target, vantage: &str, _timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let mut r = result(t, vantage, ProbeOutcome::Failed, started);
    r.error = Some(
        "this build has no `icmp` feature — the check did not run, and this is NOT \
         evidence about the target"
            .to_string(),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::probe::ProbeKind;

    fn target(kind: ProbeKind, s: &str) -> Target {
        Target {
            name: "t".into(),
            kind,
            target: s.into(),
            interval_secs: None,
            timeout_secs: None,
            expect_status: vec![],
            expect_body: None,
            follow_redirects: true,
            allow_offhost_redirect: false,
            method: None,
            headers: vec![],
            server_name: None,
            inspect_untrusted: true,
            resolver: None,
            expect_addrs: vec![],
            enabled: true,
        }
    }

    /// A closed port refuses immediately: that is `Failed`, not `Timeout`, and
    /// the two must stay distinguishable.
    #[tokio::test]
    async fn a_refused_connection_is_a_failure_not_a_timeout() {
        // Port 1 on loopback: nothing listens, and the kernel refuses at once.
        let r = tcp(
            &target(ProbeKind::Tcp, "127.0.0.1:1"),
            "test",
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(r.outcome, ProbeOutcome::Failed);
        assert!(r.error.is_some());
        assert!(r.duration_ms.unwrap() >= 0.0);
    }

    /// The 2026-08-20 signature: a connection that hangs. It must come back as
    /// `Timeout`, and the DURATION must be published — that number is what a
    /// human eventually recognised, eight days late.
    #[tokio::test]
    async fn a_black_hole_is_a_timeout_and_reports_its_duration() {
        // TEST-NET-1 (RFC 5737): routable-looking, never answers.
        let r = tcp(
            &target(ProbeKind::Tcp, "192.0.2.1:80"),
            "test",
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(r.outcome, ProbeOutcome::Timeout);
        assert!(
            r.duration_ms.unwrap() >= 250.0,
            "the duration is the diagnosis: {:?}",
            r.duration_ms
        );
        assert!(r.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_successful_connect_is_ok() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = l.accept().await;
        });
        let r = tcp(
            &target(ProbeKind::Tcp, &addr.to_string()),
            "edge",
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(r.outcome, ProbeOutcome::Ok);
        assert_eq!(r.vantage, "edge", "where it looked from is half the answer");
    }

    #[tokio::test]
    async fn a_bad_resolver_address_is_named_rather_than_silently_ignored() {
        let mut t = target(ProbeKind::Dns, "example.com");
        t.resolver = Some("not-an-address".into());
        let r = dns(&t, "test", Duration::from_millis(200)).await;
        assert_eq!(r.outcome, ProbeOutcome::Failed);
        assert!(r.error.unwrap().contains("is not a resolver address"));
    }

    /// A DNS answer must always name its resolver, even on failure — the
    /// answer alone is meaningless without knowing who gave it.
    #[tokio::test]
    async fn a_dns_failure_still_names_the_resolver() {
        let mut t = target(ProbeKind::Dns, "nonexistent.invalid");
        t.resolver = Some("192.0.2.1:53".into());
        let r = dns(&t, "test", Duration::from_millis(300)).await;
        assert_ne!(r.outcome, ProbeOutcome::Ok);
        assert_eq!(
            r.dns.unwrap().resolver.as_deref(),
            Some("192.0.2.1:53"),
            "who answered is half the finding"
        );
    }

    #[cfg(not(feature = "icmp"))]
    #[tokio::test]
    async fn icmp_without_the_feature_says_it_did_not_run() {
        let r = icmp(
            &target(ProbeKind::Icmp, "127.0.0.1"),
            "test",
            Duration::from_secs(1),
        )
        .await;
        assert!(
            r.error.unwrap().contains("NOT evidence about the target"),
            "a check that did not run must not read as a target being down"
        );
    }
}
