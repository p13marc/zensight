//! Single-writer ownership on a live bus (#1104, #1105).
//!
//! The three failures this pins, all from the original startup-only election:
//!
//! 1. a later instance with a lexically **lower** zid computed itself the
//!    winner and published beside the incumbent — every seed GET then returned
//!    two replies, and for `@desired` every sensor flapped between two
//!    configurations, silently;
//! 2. losers **exited**, so killing the owner left no service until a
//!    supervisor restarted a loser whose zid happened to sort right;
//! 3. the two service origins now share one implementation, so a fix to one is
//!    a fix to both.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::service_guard::{ServiceGuard, Standing};

const T: Duration = Duration::from_secs(2);

fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
        .insert_json5("listen/endpoints", r#"["tcp/127.0.0.1:0"]"#)
        .unwrap();
    config
}

/// Two peers on one loopback bus.
async fn pair() -> (Arc<zenoh::Session>, Arc<zenoh::Session>) {
    let a = Arc::new(zenoh::open(isolated_config()).await.expect("open a"));
    let locator = a
        .info()
        .locators()
        .await
        .into_iter()
        .next()
        .expect("a is listening");
    let mut cfg = isolated_config();
    cfg.insert_json5("connect/endpoints", &format!("[\"{locator}\"]"))
        .unwrap();
    let b = Arc::new(zenoh::open(cfg).await.expect("open b"));
    // Let the peers find each other.
    tokio::time::sleep(Duration::from_millis(400)).await;
    (a, b)
}

/// **A live incumbent outranks any zid.**
///
/// The original election compared claim chunks and ran once, so an instance
/// started later with a lower zid declared `alive` and published while the
/// incumbent kept publishing too. The tie-break is for simultaneous starts, not
/// a standing entitlement to displace a running owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_incumbent_outranks_a_lower_zid() {
    let (a, b) = pair().await;
    let (ga, gb) = (
        ServiceGuard::catalog(a.clone()),
        ServiceGuard::catalog(b.clone()),
    );

    // Roles are assigned BY ZID, not by which session opened first: the
    // incumbent takes the HIGHER zid and the challenger the lower, so under the
    // old lowest-zid-wins rule the challenger would take ownership every time.
    // Picking roles at random would make this test pass or fail on a coin flip
    // — which is exactly what it did before this line existed.
    let (incumbent, challenger) = if ga.zid() < gb.zid() {
        (&gb, &ga)
    } else {
        (&ga, &gb)
    };
    assert!(
        challenger.zid() < incumbent.zid(),
        "the challenger must sort first for this test to mean anything"
    );

    let claim = match incumbent.campaign(T).await.expect("incumbent campaigns") {
        Standing::Owner(c) => c,
        Standing::StandBy { owner } => panic!("lost an uncontested election ({owner:?})"),
    };
    let alive = incumbent.declare_alive().await.expect("incumbent alive");
    tokio::time::sleep(Duration::from_millis(300)).await;

    match challenger.campaign(T).await.expect("challenger campaigns") {
        Standing::StandBy { .. } => {}
        Standing::Owner(_) => panic!(
            "the lower zid ({}) took ownership from a live incumbent ({}) — two writers \
             on one service origin, which for @desired flaps every sensor between two \
             configurations",
            challenger.zid(),
            incumbent.zid()
        ),
    }

    // …and takes over once the owner is gone, rather than needing a restart
    // and a favourable zid.
    drop(alive);
    drop(claim);
    tokio::time::timeout(
        Duration::from_secs(10),
        challenger.wait_for_vacancy(T, Duration::from_millis(200)),
    )
    .await
    .expect("the standby never noticed the owner leave");
    match challenger
        .campaign(T)
        .await
        .expect("challenger re-campaigns")
    {
        Standing::Owner(_) => {}
        Standing::StandBy { owner } => panic!("did not take over a vacant origin ({owner:?})"),
    }
}

/// The two service origins are independent: `@desired` must never lose an
/// election to the catalog, or the deployment has one service where it has two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_origins_are_owned_independently() {
    let (a, b) = pair().await;
    let catalog = ServiceGuard::catalog(a.clone());
    let desired = ServiceGuard::desired(b.clone());

    let _c = match catalog.campaign(T).await.expect("catalog campaigns") {
        Standing::Owner(c) => c,
        Standing::StandBy { owner } => panic!("catalog lost ({owner:?})"),
    };
    let _alive = catalog.declare_alive().await.expect("catalog alive");
    tokio::time::sleep(Duration::from_millis(300)).await;

    match desired.campaign(T).await.expect("desired campaigns") {
        Standing::Owner(_) => {}
        Standing::StandBy { owner } => {
            panic!("@desired stood by for the catalog's ownership ({owner:?})")
        }
    }
}

/// An uncontested start owns the origin — the case that must keep working, or
/// the standby loop above simply never starts anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sole_instance_owns_the_origin() {
    let s = Arc::new(zenoh::open(isolated_config()).await.expect("open"));
    let g = ServiceGuard::desired(s.clone());
    assert!(g.incumbent(T).await.is_none(), "nobody owns it yet");
    match g.campaign(T).await.expect("campaign") {
        Standing::Owner(_) => {}
        Standing::StandBy { owner } => panic!("a sole instance stood by ({owner:?})"),
    }
}
