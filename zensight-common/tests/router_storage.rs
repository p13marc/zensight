//! Router-storage conformance (#471) — run against a **real `zenohd`**.
//!
//! `configs/router-*.json5` are the only part of the deployed system that never
//! had a test: they are hand-written JSON5, consumed by a binary we do not build,
//! and every claim in their comments ("survives a router restart", "answers the
//! late-joiner seed", "tombstones expire") was an assertion nobody had run. A
//! wrong `strip_prefix` or a selector that silently matches nothing produces a
//! router that starts happily and stores nothing.
//!
//! These tests spawn `zenohd` with the shipped config and assert the behaviour
//! the config comments promise. They are `#[ignore]`d — CI has no `zenohd` — and
//! run via `just router-verify`.
//!
//! **Isolation** (project rule, and the reason this suite exists at all): the
//! shipped configs listen on `0.0.0.0:7447` with default multicast scouting, i.e.
//! they are written to *join the fleet*. Running that here would join the
//! operator's live hub. Every run below overrides listen/scouting to loopback and
//! multicast-off. The *storages* — the thing under test — are used exactly as
//! shipped.
//!
//! These tests are sessions on the **wire**, so they speak full keys
//! (`zensight/v1/…`) and set no namespace, like `zenctl` and a router do. That is
//! deliberate: a test that cannot see outside the namespace cannot prove what
//! landed in the storage.

use std::process::{Child, Command};
use std::time::Duration;

/// A loopback port for the router under test. Never 7447 — that is the live hub.
const ROUTER_PORT: u16 = 17447;

/// Long enough for zenohd to load the storage-manager plugin, open its volumes,
/// and start listening. Startup is the slow part; the assertions are not.
const ROUTER_BOOT: Duration = Duration::from_secs(3);

/// Storages settle asynchronously — a PUT returns before the backend has written.
const SETTLE: Duration = Duration::from_millis(800);

/// A running `zenohd`, killed on drop so a failing assertion cannot leave a
/// router (and a listening socket) behind.
struct Router {
    child: Child,
    _dir: tempfile::TempDir,
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Router {
    /// Spawn `zenohd` with one of the shipped configs, isolated to loopback.
    ///
    /// `--cfg` overrides are applied *on top of* the config file, so the
    /// storages/volumes/timestamping under test are the shipped ones and only
    /// the transport is redirected.
    fn spawn(config: &str) -> Option<Self> {
        // The fs backend hangs its relative `dir`s under this root, so each run
        // gets a fresh, empty store: a stale directory would let a test pass on
        // the *previous* run's data.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");

        let child = Command::new("zenohd")
            .arg("-c")
            .arg(root.join("configs").join(config))
            .arg("--cfg")
            .arg(format!(
                "listen/endpoints:[\"tcp/127.0.0.1:{ROUTER_PORT}\"]"
            ))
            // Isolation: no multicast, no gossip out. The router must not find
            // the operator's fleet and the fleet must not find it.
            .arg("--cfg")
            .arg("scouting/multicast/enabled:false")
            .arg("--cfg")
            .arg("scouting/gossip/enabled:false")
            .env("ZENOH_BACKEND_FS_ROOT", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        std::thread::sleep(ROUTER_BOOT);
        Some(Router { child, _dir: dir })
    }
}

/// A client session against the router under test, and nothing else: multicast
/// off, gossip off, one explicit endpoint.
async fn client() -> zenoh::Session {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", r#""client""#)
        .expect("mode client");
    config
        .insert_json5(
            "connect/endpoints",
            &format!(r#"["tcp/127.0.0.1:{ROUTER_PORT}"]"#),
        )
        .expect("connect endpoint");
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("multicast off");
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .expect("gossip off");
    zenoh::open(config).await.expect("client session")
}

/// Collect every OK reply's (key, payload) for a GET.
async fn get_all(session: &zenoh::Session, selector: &str) -> Vec<(String, Vec<u8>)> {
    let replies = session
        .get(selector)
        .timeout(Duration::from_secs(5))
        .await
        .expect("get");
    let mut out = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            out.push((
                sample.key_expr().to_string(),
                sample.payload().to_bytes().to_vec(),
            ));
        }
    }
    out
}

/// Skip (loudly) rather than fail when `zenohd` is not installed — the suite is
/// `#[ignore]`d, so anyone running it asked for it and deserves to be told why
/// nothing happened.
macro_rules! router_or_skip {
    ($config:expr) => {
        match Router::spawn($config) {
            Some(r) => r,
            None => {
                eprintln!("SKIP: `zenohd` not on PATH — see `just router-verify`");
                return;
            }
        }
    };
}

/// **The claim**: "a GET on any state selector is answered by the router even
/// when producers sleep" (RFC 05 §4 late-joiner seed, `router-evidence-storage`
/// header).
///
/// The publisher is *gone* — session closed — before the GET runs. Any reply can
/// only have come from the storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just router-verify`"]
async fn a_state_doc_outlives_its_publisher() {
    let _router = router_or_skip!("router-evidence-storage.json5");
    let key = "zensight/v1/h-aaaabbbbcccc/state/sysinfo/health";

    {
        let sensor = client().await;
        sensor
            .put(key, b"{\"status\":\"healthy\"}".to_vec())
            .await
            .expect("put health");
        tokio::time::sleep(SETTLE).await;
        sensor.close().await.expect("close");
    }
    // The producer is now off the bus. Nothing but the storage can answer.
    tokio::time::sleep(SETTLE).await;

    let gui = client().await;
    let replies = get_all(&gui, "zensight/v1/*/state/**").await;
    assert!(
        replies
            .iter()
            .any(|(k, v)| k == key && v == b"{\"status\":\"healthy\"}"),
        "the router must serve the state doc after its publisher left \
         (late-joiner seed, RFC 05 §4); got {replies:?}"
    );
}

/// **The claim**: "DELETE tombstones retire a doc" (`router-evidence-storage`
/// §2.3). A tombstone that the storage ignores is worse than no storage: the GUI
/// would resurrect retired hosts on every reconnect, forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just router-verify`"]
async fn a_delete_tombstone_retires_the_doc() {
    let _router = router_or_skip!("router-evidence-storage.json5");
    let key = "zensight/v1/h-ddddeeeeffff/state/netlink/health";

    let sensor = client().await;
    sensor.put(key, b"alive".to_vec()).await.expect("put");
    tokio::time::sleep(SETTLE).await;

    let gui = client().await;
    assert!(
        !get_all(&gui, key).await.is_empty(),
        "sanity: the doc is stored before we delete it"
    );

    sensor.delete(key).await.expect("delete");
    tokio::time::sleep(SETTLE).await;

    let after = get_all(&gui, key).await;
    assert!(
        after.is_empty(),
        "a DELETE must retire the doc from the storage, not leave it queryable; got {after:?}"
    );
}

/// **The claim**: the `@catalog` storage exists because "`*` never matches the
/// verbatim `@catalog` chunk (D4)".
///
/// This is the config's single most load-bearing subtlety, and the one a
/// plausible "simplification" would delete. Prove *both* halves: the catalog doc
/// is stored (so the second storage is doing work), and the fleet selector does
/// **not** match it (so the second storage is not redundant).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just router-verify`"]
async fn the_catalog_needs_its_own_storage_because_star_cannot_match_it() {
    let _router = router_or_skip!("router-evidence-storage.json5");
    let entity = "zensight/v1/@catalog/state/entity/h-aaaabbbbcccc";

    {
        let catalog = client().await;
        catalog
            .put(entity, b"{\"entity_id\":\"h-aaaabbbbcccc\"}".to_vec())
            .await
            .expect("put entity");
        tokio::time::sleep(SETTLE).await;
        catalog.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let gui = client().await;

    // Half one: the `zensight-catalog` storage stored it (publisher is gone).
    let direct = get_all(&gui, "zensight/v1/@catalog/state/entity/*").await;
    assert!(
        direct.iter().any(|(k, _)| k == entity),
        "the @catalog storage must serve entity docs after the correlator left; got {direct:?}"
    );

    // Half two: the fleet selector cannot see it. If this ever starts matching,
    // the two storages overlap and one of them is silently redundant.
    let via_star = get_all(&gui, "zensight/v1/*/state/**").await;
    assert!(
        !via_star.iter().any(|(k, _)| k == entity),
        "`*` must not match the verbatim `@catalog` chunk (D4) — if it does, the \
         separate catalog storage is redundant and this config's rationale is wrong; \
         got {via_star:?}"
    );
}

/// **The claim** (`router-blob-storage` header): "sensors can PUT
/// content-addressed chunks into the router and then exit; GUIs GET them by
/// hash… because the bytes live on the router, not the sensor."
///
/// Same shape as the state test, and the same failure mode if `strip_prefix` is
/// wrong: an artifact download that hangs forever after the sensor restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just router-verify`"]
async fn blob_chunks_outlive_the_sensor_that_stored_them() {
    let _router = router_or_skip!("router-blob-storage.json5");
    // Keys spell the RFC 07 v1.7 shapes: `blake3` (the only algo zblob 0.2
    // writes — pre-0.11 `sha256/` chunks are inert, not part of this claim)
    // and a hex content root, never a name (`tree/report-1` is illegal now).
    let chunk = "zensight/v1/h-aaaabbbbcccc/@blob/store/blake3/deadbeefcafe";
    let tree = &format!("zensight/v1/h-aaaabbbbcccc/@blob/tree/{}", "ab".repeat(32));
    let bytes = b"chunk-bytes".to_vec();

    {
        let sensor = client().await;
        sensor.put(chunk, bytes.clone()).await.expect("put chunk");
        sensor
            .put(tree, b"tree-index".to_vec())
            .await
            .expect("put tree");
        tokio::time::sleep(SETTLE).await;
        sensor.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let gui = client().await;
    let got = get_all(&gui, chunk).await;
    assert_eq!(
        got.first().map(|(_, v)| v.as_slice()),
        Some(bytes.as_slice()),
        "a Tier-2 chunk must be re-fetchable from the router after its producer exits"
    );
    assert!(
        !get_all(&gui, tree).await.is_empty(),
        "the tree index must be re-fetchable too — a chunk store with no index is unreachable"
    );
}

/// **The trap** (RFC 05 §2.1 / 09 §2.2): a `complete` storage short-circuits
/// `BestMatching` and silently collapses a fleet-wide `@rpc` GET to a single
/// reply. Two sensors answer; the operator sees one; nothing errors.
///
/// The static half of this guard lives in `no_storage_claims_completeness`
/// below. This is the dynamic half: with the storage router in the path, a fleet
/// GET must still fan in to **every** replier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just router-verify`"]
async fn a_fleet_rpc_get_still_fans_in_through_the_storage_router() {
    let _router = router_or_skip!("router-evidence-storage.json5");

    // Two hosts serving the same procedure on their own origin-scoped keys.
    let origins = ["h-111111111111", "h-222222222222"];
    let mut sensors = Vec::new();
    for origin in origins {
        let session = client().await;
        let key = format!("zensight/v1/{origin}/@rpc/sysinfo/processes");
        let queryable = session
            .declare_queryable(&key)
            .await
            .expect("declare queryable");
        let reply_key = key.clone();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let _ = query
                    .reply(reply_key.clone(), origin.as_bytes().to_vec())
                    .await;
            }
        });
        sensors.push(session);
    }
    tokio::time::sleep(SETTLE).await;

    let gui = client().await;
    let replies = get_all(&gui, "zensight/v1/*/@rpc/sysinfo/processes").await;
    assert_eq!(
        replies.len(),
        2,
        "a fleet GET must reach every host. One reply means a `complete` storage \
         short-circuited the fan-in (RFC 05 §2.1) — the failure is silent and the \
         operator just sees fewer hosts; got {replies:?}"
    );
}

/// The static half of the completeness trap: no shipped storage may declare
/// itself `complete`. Cheap, runs everywhere, no `zenohd` — so it is **not**
/// ignored, and it fails in CI the moment someone adds the flag.
#[test]
fn no_storage_claims_completeness() {
    let configs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("configs");
    for entry in std::fs::read_dir(&configs).expect("read configs/") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with("router-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read config");
        // Strip `//` comments: the word appears in prose explaining the ban.
        let code: String = text
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("complete"),
            "{name}: a `complete` storage short-circuits BestMatching and collapses \
             fleet-wide @rpc GETs to one reply (RFC 05 §2.1). If you need one, its \
             selector must provably not intersect any @rpc fan-in path — say so here."
        );
    }
}
