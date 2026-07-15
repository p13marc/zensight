//! Guard tests: the RFC's design properties D1–D6 (03 §4), pinned as
//! executable key algebra per the RFC's own requirement. Uses an explicit
//! `zensight` base — these tests exercise full on-wire keys, the view
//! router-side artifacts (storage selectors, ACL rules) see.

use zenoh_keyexpr::keyexpr;

fn ke(s: &str) -> &keyexpr {
    keyexpr::new(s).unwrap_or_else(|e| panic!("illegal key {s}: {e}"))
}

fn inter(a: &str, b: &str) -> bool {
    ke(a).intersects(ke(b))
}

/// ACL semantics: rule ⊇ message key (inclusion).
fn incl(rule: &str, key: &str) -> bool {
    ke(rule).includes(ke(key))
}

const EXAMPLES: &[&str] = &[
    "zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage",
    "zensight/v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime",
    "zensight/v1/h-3fa9c2d41b7e/state/netring/health",
    "zensight/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1",
    "zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7",
    "zensight/v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd",
    "zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets",
    "zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high",
    "zensight/v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56",
    "zensight/v1/@catalog/state/entity/h-3fa9c2d41b7e",
    "zensight/v1/@catalog/state/alias/h-9d02aa17c44f",
    "zensight/v1/@catalog/state/pdns/93-184-216-34",
    "zensight/v1/h-3fa9c2d41b7e/state/netlink/alive",
    "zensight/v1/@catalog/state/alive",
    "zensight/v1/@catalog/state/claim/a0b1c2d3e4f5",
];

#[test]
fn example_set_is_legal_and_canon() {
    for k in EXAMPLES {
        assert_eq!(ke(k).as_str(), *k, "not canon: {k}");
    }
}

/// D1 — **version isolation**: a selector written against one convention major
/// can never match another's keys.
///
/// This replaces the original "version *hermeticity*", which was the stronger
/// claim that the *pre-v1* keyspace and v1 were mutually invisible. That one
/// needed the version chunk to be verbatim (`@v1`), because `**` does not cross
/// an `@` — so the un-versioned legacy firehose `zensight/**` could not see v1.
///
/// The `@` is gone (see `grammar::VERSION_CHUNK`): it made every zenoh-ext
/// `@adv` publisher-detection token unparseable, silently killing the advanced
/// tier's late-publisher detection. The trade is sound because the property
/// that does real work — *v1 cannot see v2, v2 cannot see v1* — never needed the
/// `@` at all: `v1` and `v2` are simply different literal chunks.
///
/// What we gave up is invisibility to an **un-versioned** selector, and the only
/// such selector was the pre-v1 keyspace's, which is retired (no shipped
/// component publishes or subscribes to it). `zensight/**` now matches v1 keys —
/// deliberately, and asserted below so the change is a decision, not a drift.
#[test]
fn d1_version_isolation() {
    // The property that matters: cross-major selectors never intersect.
    for k in EXAMPLES {
        let v2 = k.replace("zensight/v1/", "zensight/v2/");
        assert!(
            !inter("zensight/v1/**", &v2),
            "a v1 selector must not see the v2 key {v2}"
        );
        assert!(
            !inter("zensight/v2/**", k),
            "a v2 selector must not see the v1 key {k}"
        );
    }
    // Class selectors isolate across majors too, not just the firehose.
    assert!(!inter(
        "zensight/v1/*/telemetry/**",
        "zensight/v2/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
    ));

    // Legacy control selectors still cannot reach v1 — the version chunk is a
    // different *literal*, which is all that was ever required here.
    assert!(!inter(
        "zensight/*/@/**",
        "zensight/v1/h-3fa9c2d41b7e/state/netring/health"
    ));
    assert!(!inter(
        "zensight/_meta/**",
        "zensight/v1/@catalog/state/entity/h-3fa9c2d41b7e"
    ));

    // The one thing we knowingly gave up: an un-versioned firehose now reaches
    // v1. Pinned so nobody rediscovers it as a surprise — and so that anyone
    // reintroducing a pre-v1 consumer learns it here rather than on the wire.
    assert!(
        inter(
            "zensight/**",
            "zensight/v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage"
        ),
        "with a plain version chunk, an un-versioned firehose DOES see v1 \
         (the retired legacy keyspace was the only such consumer)"
    );
}

/// D2 — the per-origin firehose matches all data-class keys and no plane keys.
#[test]
fn d2_firehose_is_data_only() {
    let fire = "zensight/v1/h-3fa9c2d41b7e/**";
    for k in EXAMPLES.iter().filter(|k| k.contains("/h-3fa9c2d41b7e/")) {
        let is_plane = k.contains("/@rpc/") || k.contains("/@media/") || k.contains("/@blob/");
        assert_eq!(inter(fire, k), !is_plane, "{k}");
    }
    // @adv sidecars are invisible to the firehose too (04 §3.3).
    let sidecar = "zensight/v1/h-3fa9c2d41b7e/state/netring/health/@adv/pub/a0b1/42/m";
    assert!(!inter(fire, sidecar));
}

/// D3 — class selectors are pairwise disjoint.
#[test]
fn d3_class_disjointness() {
    let classes = [
        "zensight/v1/*/telemetry/**",
        "zensight/v1/*/state/**",
        "zensight/v1/*/events/**",
    ];
    for (i, a) in classes.iter().enumerate() {
        for (j, b) in classes.iter().enumerate() {
            assert_eq!(inter(a, b), i == j);
        }
    }
}

/// D4 — `*` never reaches a verbatim service origin.
#[test]
fn d4_service_exclusion() {
    assert!(!inter(
        "zensight/v1/*/state/**",
        "zensight/v1/@catalog/state/entity/x"
    ));
    assert!(!inter(
        "zensight/v1/*/state/*/alive",
        "zensight/v1/@catalog/state/alive"
    ));
    assert!(!inter(
        "zensight/v1/*/@rpc/**",
        "zensight/v1/@catalog/@rpc/names"
    ));
}

/// D5 — one key shape serves targeted and fleet RPC.
#[test]
fn d5_targeted_and_fleet_rpc() {
    assert!(inter(
        "zensight/v1/*/@rpc/netlink/sockets",
        "zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets"
    ));
    assert!(!inter(
        "zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets",
        "zensight/v1/h-aaaaaaaaaaaa/@rpc/netlink/sockets"
    ));
}

/// D6 preconditions — ACL inclusion: per-plane rules are necessary (a data
/// `**` rule covers no plane) and sufficient (each plane's literal-prefix
/// rule covers its keys). RFC 03 §4 D6, 09 §3.
#[test]
fn d6_acl_inclusion_per_plane() {
    let host = "zensight/v1/h-3fa9c2d41b7e";
    // The data rule covers data + alive tokens...
    assert!(incl(
        &format!("{host}/**"),
        &format!("{host}/state/netlink/alert/9f2c81ab04d7e3f1")
    ));
    assert!(incl(
        &format!("{host}/**"),
        &format!("{host}/state/netlink/alive")
    ));
    // ...but no plane and no sidecar.
    assert!(!incl(
        &format!("{host}/**"),
        &format!("{host}/@rpc/netlink/sockets")
    ));
    assert!(!incl(
        &format!("{host}/**"),
        &format!("{host}/@media/parallax/cam0/video/h264/high")
    ));
    assert!(!incl(
        &format!("{host}/**"),
        &format!("{host}/@blob/store/sha256/ab12cd34ef56")
    ));
    assert!(!incl(
        &format!("{host}/**"),
        &format!("{host}/state/x/y/@adv/pub/a/1/m")
    ));
    // Per-plane literal-prefix rules cover them.
    assert!(incl(
        &format!("{host}/@rpc/**"),
        &format!("{host}/@rpc/netlink/sockets")
    ));
    assert!(incl(
        &format!("{host}/@media/**"),
        &format!("{host}/@media/parallax/cam0/video/h264/high")
    ));
    assert!(incl(
        &format!("{host}/@blob/**"),
        &format!("{host}/@blob/store/sha256/ab12cd34ef56")
    ));
    assert!(incl(
        &format!("{host}/**/@adv/**"),
        &format!("{host}/state/x/y/@adv/pub/a/1/m")
    ));
    // Fleet RPC rule covers hosts, never @catalog.
    assert!(incl(
        "zensight/v1/*/@rpc/**",
        "zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets"
    ));
    assert!(!incl(
        "zensight/v1/*/@rpc/**",
        "zensight/v1/@catalog/@rpc/names"
    ));
    // A rule must include the declared selector, not merely intersect it.
    assert!(incl("zensight/v1/**", "zensight/v1/*/state/**"));
    assert!(!incl("zensight/v1/**", "zensight/**"));
}

/// Canonical selector precision (RFC 03 §5 table + 09 §1 cookbook).
#[test]
fn selector_precision() {
    assert!(inter(
        "zensight/v1/*/state/*/alert/*",
        "zensight/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1"
    ));
    assert!(!inter(
        "zensight/v1/*/state/*/alert/*",
        "zensight/v1/h-3fa9c2d41b7e/state/netring/health"
    ));
    assert!(inter(
        "zensight/v1/*/state/*/evidence/**",
        "zensight/v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7"
    ));
    // Media viewers subscribe to an EXACT tier — no wildcard on @media (07 §1,
    // keyspace v1.3): an exact-tier key never matches a sibling stream…
    assert!(!inter(
        "zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high",
        "zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam1/video/h264/high"
    ));
    // …and, the property the wildcard revocation exists for, never matches a
    // SIBLING TIER of the same stream (a `…/h264/*` would have matched both,
    // interleaving two H.264 streams on one subscriber and breaking simulcast).
    assert!(!inter(
        "zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high",
        "zensight/v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/low"
    ));
    // Sibling-suffix subject versioning stays invisible to pinned selectors
    // (RFC 08 §3: sockets2, never sockets/v2).
    assert!(!inter(
        "zensight/v1/*/@rpc/netlink/sockets",
        "zensight/v1/h-3fa9c2d41b7e/@rpc/netlink/sockets2"
    ));
}

/// Crate builders emit exactly the base-relative forms of the example set.
#[test]
fn builders_match_examples() {
    use zensight_keyspace::grammar::{self, Class, Origin, Producer};
    use zensight_keyspace::origin::HostId;

    let host = Origin::Host(HostId::parse("h-3fa9c2d41b7e").unwrap());
    let netlink = Producer::new("netlink").unwrap();
    let built = grammar::data_key(
        &host,
        Class::State,
        Some(&netlink),
        &["alert", "9f2c81ab04d7e3f1"],
    )
    .unwrap();
    assert_eq!(
        grammar::with_base("zensight", &built),
        "zensight/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1"
    );
    // Built keys are canon zenoh keyexprs.
    ke(&grammar::with_base("zensight", &built));
}
