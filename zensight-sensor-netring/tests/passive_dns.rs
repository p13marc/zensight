//! Passive-DNS + FQDN-beacon pcap replay coverage (issue #308).
//!
//! Replays `tests/fixtures/passive_dns.pcap` (generated offline by
//! `scripts/gen-dns-pcap.py`; the fixture is committed) through the full
//! monitor build: a CNAME-chained A answer + a PTR answer, then 16
//! identical-size TCP flows from the resolving client to the resolved IP,
//! exactly 30 s apart. Asserts the whole #308 surface end-to-end:
//!
//! - flow records to the resolved IP carry `dst_names` with the *queried*
//!   name (CNAME chains bind the queried label, not the intermediate) and
//!   forward-DNS provenance;
//! - the FQDN-pivoted RITA beacon fires, keyed by that name;
//! - the name map stays bounded and its claims carry the DNS *client* (the
//!   non-port-53 flow endpoint) — the empirical orientation check.

use zensight_sensor_netring::config::NetringSensorConfig;
use zensight_sensor_netring::monitor;

#[tokio::test]
async fn passive_dns_replay_enriches_flows_and_fires_fqdn_beacon() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/passive_dns.pcap"
    );
    // 16 beacons over 450 s put the RITA composite at ~0.85 (perfect ts/ds
    // scores, partial duration bonus) — threshold 0.8 keeps a real margin on
    // both sides of the default 0.9.
    let cfg: NetringSensorConfig = json5::from_str(&format!(
        r#"{{
            netring: {{
                source: "pdns-test",
                pcap: "{fixture}",
                collect: {{ dns: true }},
                anomalies: {{ rita_beacon_fqdn: true, rita_beacon_threshold: 0.8 }},
                names: {{ max_ips: 64 }},
            }},
        }}"#
    ))
    .expect("test config parses");

    let (mon, mut channels, keepalive, _handle) =
        monitor::build(&cfg.netring).expect("monitor builds");
    mon.replay().await.expect("pcap replay");
    drop(keepalive);

    // (a) Flow enrichment: every ended flow to the resolved IP names it after
    // the QUERIED label (CNAME chain bound to beacon.evil.example, not the
    // cdn.evil.example intermediate), with forward-DNS provenance.
    let flows = channels.flow_records.lock().unwrap();
    let beacon_flows: Vec<_> = flows
        .iter()
        .filter(|f| f.dst.starts_with("93.184.216.34"))
        .collect();
    assert!(
        !beacon_flows.is_empty(),
        "expected ended flows to the beacon destination, got: {:?}",
        flows.iter().map(|f| &f.dst).collect::<Vec<_>>()
    );
    for f in &beacon_flows {
        assert_eq!(
            f.src.split(':').next(),
            Some("10.0.0.5"),
            "initiator must be the client"
        );
        assert_eq!(
            f.dst_names.len(),
            1,
            "one name claim expected, got {:?}",
            f.dst_names
        );
        assert_eq!(f.dst_names[0].name, "beacon.evil.example");
        assert_eq!(f.dst_names[0].provenance, "dns_a");
    }
    drop(flows);

    // (b) The FQDN beacon anomaly fired, keyed by the name.
    let mut fqdn_alerts = Vec::new();
    while let Ok(alert) = channels.alerts.try_recv() {
        if alert.rule == "RitaBeaconFqdn" {
            fqdn_alerts.push(alert);
        }
    }
    assert!(
        !fqdn_alerts.is_empty(),
        "expected at least one RitaBeaconFqdn alert"
    );
    let alert = &fqdn_alerts[0];
    assert_eq!(
        alert.labels.get("fqdn").map(String::as_str),
        Some("beacon.evil.example")
    );
    assert_eq!(
        alert.labels.get("src").map(String::as_str),
        Some("10.0.0.5")
    );
    assert_eq!(
        alert.labels.get("dst").map(String::as_str),
        Some("93.184.216.34")
    );
    assert_eq!(
        alert.labels.get("technique").map(String::as_str),
        Some("T1071")
    );
    let score: f64 = alert.labels.get("score").unwrap().parse().unwrap();
    assert!(
        score >= 0.8,
        "composite score must clear the configured threshold, got {score}"
    );
    // The per-name cooldown (300 s event time) bounds re-alerting: the beacon
    // scores from packet 10 onward (t=280..460), so at most 2 emissions fit.
    assert!(
        fqdn_alerts.len() <= 2,
        "cooldown must bound re-alerts, got {}",
        fqdn_alerts.len()
    );

    // (c) Name map: bounded, and the claims carry the DNS *client* — the
    // non-port-53 endpoint of the DNS flow (the orientation check).
    let nm = channels.name_map.as_ref().expect("name map built").clone();
    let map = nm.lock().unwrap();
    assert!(map.len() <= 64, "name map exceeds max_ips: {}", map.len());
    // Fixture DNS lands at t=BASE+0.05..0.15 with TTL 86400 — peek within it.
    let t = flowscope::Timestamp::new(1_700_000_000 + 500, 0);
    let client: std::net::IpAddr = "10.0.0.5".parse().unwrap();

    let forward = map.peek_names("93.184.216.34".parse().unwrap(), t);
    assert_eq!(forward.len(), 1, "one forward claim, got {forward:?}");
    assert_eq!(forward[0].name, "beacon.evil.example");
    assert_eq!(
        forward[0].client,
        Some(client),
        "claims must be scoped to the DNS client, not the resolver"
    );

    let reverse = map.peek_names("10.0.0.9".parse().unwrap(), t);
    assert_eq!(reverse.len(), 1, "one PTR claim, got {reverse:?}");
    assert_eq!(reverse[0].name, "ptr.evil.example");
    assert_eq!(reverse[0].provenance.as_str(), "dns_ptr");
    assert_eq!(reverse[0].client, Some(client));
}
