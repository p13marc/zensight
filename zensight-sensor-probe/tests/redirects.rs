//! The redirect contract, against a real server (#1134).
//!
//! Three claims the crate made and did not keep:
//!
//! 1. `follow_redirects: false` means **do not follow**. reqwest's policy is
//!    per-client and this is per-target, so the shared client followed
//!    everything: a target written to catch an endpoint that starts answering
//!    `302 → /login` followed it, got 200 from the login page and reported
//!    **up**.
//! 2. `redirects` is the **chain**, as the README says. It was "the final URL
//!    if it differs from the configured string", so `https://example.com`
//!    reported a redirect to `https://example.com/` every poll — reqwest's
//!    own normalisation, read as a redirect.
//! 3. The off-host comparison is case-insensitive and bracket-safe. A target
//!    written `Example.com` reported a redirect against its own answer, and
//!    `https://[::1]:8443/` reported one forever.

use std::time::Duration;

use axum::response::Redirect;
use axum::{Router, routing::get};
use zensight_common::probe::{ProbeKind, ProbeOutcome};
use zensight_sensor_probe::check;
use zensight_sensor_probe::config::Target;

fn target(name: &str, kind: ProbeKind, t: &str) -> Target {
    Target {
        count: None,
        spacing_ms: None,
        transport: None,
        name: name.into(),
        kind,
        target: t.into(),
        interval_secs: None,
        timeout_secs: Some(2),
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

/// **The poller's own client**, not a copy of it. The bug was one line in
/// that builder, so a test that constructed its own would prove nothing about
/// the sensor: on the parent commit this returns a client with
/// `Policy::limited(10)`, and `a_target_that_says_not_to_follow_is_not_followed`
/// fails because of it.
fn client() -> reqwest::Client {
    zensight_sensor_probe::poller::http_client(Duration::from_secs(5)).unwrap()
}

async fn spawn() -> std::net::SocketAddr {
    let app = Router::new()
        .route("/ok", get(|| async { "fine" }))
        // The shape the issue is about: an endpoint that starts sending you
        // to a login page.
        .route("/guarded", get(|| async { Redirect::temporary("/login") }))
        .route("/login", get(|| async { "please sign in" }))
        // Two hops, so "the chain" is distinguishable from "where it ended".
        .route("/hop1", get(|| async { Redirect::temporary("/hop2") }))
        .route("/hop2", get(|| async { Redirect::temporary("/ok") }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    addr
}

/// **The acceptance.** `follow_redirects: false` against a 302 reports down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_that_says_not_to_follow_is_not_followed() {
    let addr = spawn().await;
    let mut t = target(
        "guarded",
        ProbeKind::Http,
        &format!("http://{addr}/guarded"),
    );
    t.follow_redirects = false;
    t.expect_status = vec![200];

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert_eq!(
        r.outcome,
        ProbeOutcome::Failed,
        "a 302 where 200 was required is down, whatever is behind the \
         redirect: {:?}",
        r.error
    );
    let http = r.http.expect("an http result");
    assert_eq!(http.status, Some(307), "the 3xx itself is the answer");
    assert!(
        http.redirects.is_empty(),
        "nothing was followed, so there is no chain: {:?}",
        http.redirects
    );
}

/// The same target, following: it lands on the login page and passes — which
/// is the behaviour the flag exists to turn OFF, and is what happened either
/// way before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn following_is_still_the_default() {
    let addr = spawn().await;
    let mut t = target(
        "guarded",
        ProbeKind::Http,
        &format!("http://{addr}/guarded"),
    );
    t.expect_status = vec![200];

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert_eq!(r.outcome, ProbeOutcome::Ok);
    let http = r.http.expect("an http result");
    assert_eq!(http.status, Some(200));
    assert_eq!(
        http.redirects,
        vec![format!("http://{addr}/login")],
        "the hop is recorded"
    );
}

/// `redirects` is every hop, in order — not just where it stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_chain_is_recorded_in_order() {
    let addr = spawn().await;
    let t = target("chain", ProbeKind::Http, &format!("http://{addr}/hop1"));

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert_eq!(r.outcome, ProbeOutcome::Ok);
    assert_eq!(
        r.http.expect("an http result").redirects,
        vec![format!("http://{addr}/hop2"), format!("http://{addr}/ok")],
        "the README promises the chain, and only the last URL was recorded"
    );
}

/// **The acceptance.** Normalisation is not a redirect. A target written
/// without a path answers with one, and that is the same URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_normalisation_is_not_a_redirect() {
    let addr = spawn().await;
    // No trailing slash and no path: what a URL parser normalises.
    let t = target("root", ProbeKind::Http, &format!("http://{addr}"));

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert!(
        r.http.expect("an http result").redirects.is_empty(),
        "`http://host` answering as `http://host/` is the parser, not the \
         server"
    );
}

/// **The acceptance.** The host comparison is case-insensitive, so a target
/// written with capitals does not report a redirect against its own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_off_host_test_ignores_case() {
    let addr = spawn().await;
    let mut t = target("cased", ProbeKind::Http, &format!("http://{addr}/hop1"));
    // What an operator types. `127.0.0.1` has no case, so assert the rule at
    // the level it lives: `Target::host()` lowercases.
    t.server_name = Some("EXAMPLE.com".into());
    assert_eq!(t.host().as_deref(), Some("example.com"));

    t.server_name = None;
    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert_eq!(
        r.outcome,
        ProbeOutcome::Ok,
        "a same-host chain must not read as off-host: {:?}",
        r.error
    );
}

/// **The acceptance.** An IPv6 literal keeps its brackets out of the host, so
/// `https://[::1]:8443/` does not report a permanent off-host redirect.
#[test]
fn an_ipv6_literal_target_yields_its_address_not_a_fragment() {
    for (target_str, want) in [
        ("https://[::1]:8443/healthz", "::1"),
        ("https://[2001:db8::1]/", "2001:db8::1"),
        ("https://example.com:8443/x", "example.com"),
        ("https://example.com/x", "example.com"),
    ] {
        let t = target("v6", ProbeKind::Http, target_str);
        assert_eq!(
            t.host().as_deref(),
            Some(want),
            "host() of {target_str} — the port split used to cut the address \
             itself (#1134)"
        );
    }
}

// ── Burst resolution (#1135) ────────────────────────────────────────────────
//
// The burst path parsed an `IpAddr` and returned `false` for anything else —
// and `false` from one probe is a **lost packet**. `validate()` only requires
// `host:port` for tcp, so an icmp burst target is a bare host by design:
// `{kind: "burst", transport: "icmp", target: "gw.example.net"}` published
// `loss_pct: 100` every interval and a critical `probe-down` over a healthy
// link.
//
// The tcp transport is exercised here rather than icmp because ICMP needs
// `CAP_NET_RAW` and the feature is off by default — but the resolution is the
// same code for both, and the two tests below pin both halves of it.

fn burst_target(name: &str, t: &str, transport: &str) -> Target {
    let mut b = target(name, ProbeKind::Burst, t);
    b.transport = Some(transport.into());
    b.count = Some(3);
    b.spacing_ms = Some(1);
    b
}

/// **The acceptance, in the shape CI can run.** A burst against a NAME — not
/// an address — reports what actually happened, not 100 % loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_against_a_name_is_resolved_not_counted_as_loss() {
    let addr = spawn().await;
    let t = burst_target("gw", &format!("localhost:{}", addr.port()), "tcp");

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    let burst = r.burst.expect("a burst result");
    assert_eq!(
        burst.received, 3,
        "every probe answered; a name is not packet loss (#1135). error={:?}",
        r.error
    );
    assert_eq!(burst.loss_pct, 0.0);
    assert_eq!(r.outcome, ProbeOutcome::Ok);
}

/// A name that does not resolve fails the **check**, and says so. It is a
/// different fact from "nothing answered", and reporting the first as the
/// second is what made a healthy link read as down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_that_does_not_resolve_fails_the_check_not_the_packets() {
    let t = burst_target("nope", "no-such-host.invalid:9", "tcp");

    let r = check::run(&t, "here", Duration::from_secs(2), &client()).await;
    assert_eq!(r.outcome, ProbeOutcome::Failed);
    assert!(
        r.burst.is_none(),
        "no packets were sent, so there is no loss figure to publish"
    );
    let err = r.error.unwrap_or_default();
    assert!(
        err.contains("no-such-host.invalid"),
        "the error must name the target that did not resolve: {err:?}"
    );
    assert!(
        !err.contains("lost or timed out"),
        "a resolution failure must not be reported as packet loss: {err:?}"
    );
}
