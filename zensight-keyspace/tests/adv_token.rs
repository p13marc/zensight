//! The zenoh-ext `@adv` publisher-detection token must be parseable.
//!
//! This is the test that keeps the advanced tier alive, and it is why
//! [`VERSION_CHUNK`](zensight_keyspace::grammar::VERSION_CHUNK) is a plain `v1`
//! rather than a verbatim `@v1`.
//!
//! zenoh-ext parks a liveliness token at `<key>/@adv/pub/<zid>/<eid>/<meta>` for
//! every publisher declared with `publisher_detection()`, and parses it back
//! with (zenoh-ext 1.9, `src/advanced_cache.rs:39`):
//!
//! ```text
//! ${remaining:**}/@adv/${entity:*}/${zid:*}/${eid:*}/${meta:**}
//! ```
//!
//! The publisher's own key has to be captured by `${remaining:**}` — and `**`
//! never matches a chunk beginning with `@`. So while the version chunk was
//! `@v1`, **every** token we declared was unparseable by the only code that
//! reads them: `detect_late_publishers()` was silently dead, and each subscriber
//! logged "malformed liveliness token key expression" per publisher, forever.
//!
//! No upstream fix could have helped: the `@`-exclusion is a Zenoh *matching*
//! rule, so no keformat spelling can capture a key containing a verbatim chunk.
//! The fix had to be on our side.
//!
//! Note what this does **not** ask for. `@rpc`/`@media`/`@blob` and `@catalog`
//! stay verbatim — they carry design properties D2 and D4, and no
//! AdvancedPublisher ever publishes on them (the catalog uses a plain
//! `declare_publisher`). Only the version chunk had to give.

use zenoh::key_expr::format::kedefine;
use zenoh::key_expr::keyexpr;
use zensight_keyspace::grammar::VERSION_CHUNK;

// Verbatim copy of zenoh-ext 1.9's `ke_liveliness` — it is `pub(crate)` there,
// so replicating the format is the only way to pin behaviour we depend on. If a
// future zenoh-ext changes the format, this test still states the contract we
// need it to keep.
kedefine!(
    pub ke_liveliness: "${remaining:**}/@adv/${entity:*}/${zid:*}/${eid:*}/${meta:**}",
);

/// The token suffix zenoh-ext appends (`uhlc` is the eid a timestamped
/// AdvancedPublisher declares — the value `detect_late_publishers()` matches on).
const ADV: &str = "/@adv/pub/9040a1aede048880bc9d045548d4d0a1/uhlc/_";

/// The whole point: a token on a real telemetry key parses, and `remaining`
/// comes back as the publisher's key.
#[test]
fn adv_token_parses_on_a_data_key() {
    let key = format!("zensight/{VERSION_CHUNK}/h-9706b31ddad3/telemetry/netring/tls/pq_ratio");
    let token = format!("{key}{ADV}");

    let parsed = ke_liveliness::parse(keyexpr::new(token.as_str()).unwrap()).unwrap_or_else(|_| {
        panic!(
            "zenoh-ext cannot parse the @adv token for {key}.\n\
             If the version chunk became verbatim again, this is why that is not \
             allowed: publisher detection dies silently and every subscriber's log \
             fills with 'malformed liveliness token key expression'. \
             See grammar::VERSION_CHUNK."
        )
    });

    assert_eq!(parsed.remaining().unwrap().as_str(), key);
    assert_eq!(parsed.zid().as_str(), "9040a1aede048880bc9d045548d4d0a1");
}

/// The regression, stated as its counterfactual: make the version chunk verbatim
/// again and the very same token stops parsing. This reproduces the bug we
/// shipped in one line, so the reason for the plain chunk cannot be lost.
#[test]
fn a_verbatim_version_chunk_would_break_it_again() {
    let good =
        format!("zensight/{VERSION_CHUNK}/h-9706b31ddad3/telemetry/netring/tls/pq_ratio{ADV}");
    let bad = format!("zensight/@v1/h-9706b31ddad3/telemetry/netring/tls/pq_ratio{ADV}");

    assert!(ke_liveliness::parse(keyexpr::new(good.as_str()).unwrap()).is_ok());
    assert!(
        ke_liveliness::parse(keyexpr::new(bad.as_str()).unwrap()).is_err(),
        "`**` is supposed to be unable to cross a verbatim chunk — that Zenoh rule \
         is what D2/D4 rely on, and what the version chunk must therefore avoid"
    );
}

/// `VERSION_CHUNK` itself must stay wildcard-reachable: a one-line guard against
/// the whole class of mistake, independent of zenoh-ext's format.
#[test]
fn the_version_chunk_is_wildcard_reachable() {
    assert!(
        !VERSION_CHUNK.starts_with('@'),
        "the version chunk must not be verbatim — see grammar::VERSION_CHUNK"
    );
    let star = keyexpr::new("zensight/*/h-9706b31ddad3/**").unwrap();
    let key = format!("zensight/{VERSION_CHUNK}/h-9706b31ddad3/telemetry/sysinfo/cpu/usage");
    assert!(
        star.intersects(keyexpr::new(key.as_str()).unwrap()),
        "a `*` must be able to match the version chunk"
    );
}
