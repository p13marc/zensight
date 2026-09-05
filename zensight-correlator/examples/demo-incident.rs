//! The scripted fault behind `just demo-incident` (#945): a hypervisor dies and
//! takes a guest with it, on a real bus, in about a minute.
//!
//! # Why this exists
//!
//! #945 is the acceptance test of two epics. 0.15.0 put the relationship graph
//! on the bus and 0.16.0 put incidents in the catalog, and both are *invisible*
//! without a fault to look at: a healthy fleet renders the same in ZenSight as
//! it does in Grafana. The difference only shows when something breaks and one
//! of the two says **which** thing broke.
//!
//! So this publishes a fleet that is about to have a bad minute:
//!
//! ```text
//!   pve01  (hypervisor)  ──Hosts──▶  vm101  (guest)
//!     │                                │
//!     └─ liveliness token              └─ a firing alert
//!        …dropped at the fault
//! ```
//!
//! and the catalog concludes what no series can: `vm101`'s alert is a
//! **symptom_of** `pve01`. Grafana, given the same three series, shows three
//! lines and no relationship — which is the demo's whole point, and why the
//! issue asks for the two side by side.
//!
//! # It is a publisher, not a mock
//!
//! Every document goes on the wire in its shipped shape — `HostEvidence`,
//! `RelationshipEvidence`, `Alert` — through declared publishers, and the
//! liveliness tokens are real Zenoh tokens. A **real** correlator subscribes
//! them, runs the real union-find, resolves the real edge and publishes the
//! real incident. Nothing here reaches into the correlator, and there is no
//! path in this file that can produce an incident the fleet could not.
//!
//! That is the difference between a demo and a screenshot, and it is what lets
//! `scripts/demo-incident-verify.sh` assert on the result in CI.
//!
//! # Two synthetic origins, minted deliberately
//!
//! A demo needs two hosts and CI has one, so both origins are minted with
//! `V1Context::with_origin` — the constructor zenkey documents as being "for
//! tests, and for consumers that mint their identity differently". They are
//! fixed `h-…` ids rather than derived, so every run of the demo produces the
//! same entity ids and the verify script can say what it expects.
//!
//! ```bash
//! DEMO_CONNECT=tcp/127.0.0.1:7447 FAULT_AFTER_SECS=20 HOLD_SECS=60 \
//!     cargo run -p zensight-correlator --example demo-incident
//! ```

use std::sync::Arc;
use std::time::Duration;

use zenkey::grammar::Origin;
use zenkey::origin::HostId;
use zensight_common::relation::{EndpointClaim, RelationKind, RelationshipEvidence};
use zensight_common::serialization::Format;
use zensight_common::v1::{V1Context, V1ContextExt};
use zensight_common::{
    Alert, AlertKind, AlertSeverity, HostEvidence, Protocol, current_timestamp_millis,
};

/// The hypervisor's origin chunk. Fixed so entity ids are stable run to run.
const HYPERVISOR_ORIGIN: &str = "h-9e5100000001";
/// The guest's origin chunk.
const GUEST_ORIGIN: &str = "h-9e5100000002";

/// The stable `host_id` each host self-reports. These are what the relation
/// claim points at, and what the catalog joins on — the origin chunk is a key
/// chunk, the host_id is identity, and conflating them is how a demo teaches
/// the wrong model.
const HYPERVISOR_HOST_ID: &str = "demo-hypervisor-pve01";
const GUEST_HOST_ID: &str = "demo-guest-vm101";

#[tokio::main]
async fn main() {
    let connect = std::env::var("DEMO_CONNECT").unwrap_or_else(|_| "tcp/127.0.0.1:7447".into());
    let fault_after = env_secs("FAULT_AFTER_SECS", 20);
    let hold = env_secs("HOLD_SECS", 60);

    let session = Arc::new(open(&connect).await);

    let hyp = ctx(HYPERVISOR_ORIGIN, "sysinfo");
    let hyp_pve = ctx(HYPERVISOR_ORIGIN, "pve");
    let guest = ctx(GUEST_ORIGIN, "sysinfo");

    // ── the fleet, healthy ──────────────────────────────────────────────
    //
    // Self-reports first: the relation claim below points at these host_ids,
    // and an edge whose ends resolve to nothing is dropped by the catalog
    // rather than half-drawn.
    put(
        &session,
        hyp.const_state_key(&["evidence", "self"]).as_ref(),
        &self_report(HYPERVISOR_HOST_ID, "pve01", "sysinfo"),
    )
    .await;
    put(
        &session,
        guest.const_state_key(&["evidence", "self"]).as_ref(),
        &self_report(GUEST_HOST_ID, "vm101", "sysinfo"),
    )
    .await;

    // The edge, claimed by the hypervisor's `pve` producer exactly as the real
    // sensor claims it: `from` contains, `to` is contained.
    let relation = RelationshipEvidence {
        sensor: "pve".into(),
        source: "pve01".into(),
        kind: RelationKind::Hosts,
        from: EndpointClaim::host(HYPERVISOR_HOST_ID),
        to: EndpointClaim::host(GUEST_HOST_ID),
        attrs: Default::default(),
        last_updated: current_timestamp_millis(),
    };
    let relation_key = hyp_pve
        .state_key(&["evidence", "relation", &relation.relation_id()])
        .expect("relation_id is a legal chunk");
    put(&session, relation_key.as_ref(), &relation).await;

    // Liveliness for both, so the catalog has something to lose. Declared
    // AFTER the evidence: a token whose origin the catalog cannot map to an
    // entity yet tells it nothing, and the demo should not depend on which
    // subscriber happened to be ready first.
    let hyp_token = token(&session, &hyp).await;
    let _guest_token = token(&session, &guest).await;

    // The guest's alert. It fires while everything is alive, and that is the
    // point: on its own it is an unexplained alert on a VM, and an operator
    // would go and look at the VM. What changes in twenty seconds is not the
    // alert — it is what the catalog can say about it.
    let alert = guest_alert();
    let alert_key = guest
        .state_key(&["alert", &alert.alert_key()])
        .expect("alert_key is a legal chunk");
    put(&session, alert_key.as_ref(), &alert).await;

    println!("demo-incident: fleet up on {connect}");
    println!("  hypervisor  {HYPERVISOR_ORIGIN}  (pve01)  — alive, hosts vm101");
    println!("  guest       {GUEST_ORIGIN}  (vm101)  — alive, one firing alert");
    println!("  the alert is UNEXPLAINED: nothing is down, so it is nobody's symptom.");
    println!("\ndemo-incident: the hypervisor dies in {fault_after}s…");
    tokio::time::sleep(Duration::from_secs(fault_after)).await;

    // ── the fault ───────────────────────────────────────────────────────
    //
    // Dropping the token IS the fault. A machine that stops answering
    // publishes no alert about itself — the absence of its token is the only
    // evidence it is the cause, which is exactly what the catalog attributes
    // through. Nothing else changes: the guest's alert is the same alert, on
    // the same key, with the same payload.
    drop(hyp_token);
    println!("demo-incident: pve01's liveliness token is gone — it is down.");
    println!("  the catalog should now file vm101's alert as symptom_of pve01.");
    println!("  Grafana, given the same series, shows the same two lines and no relationship.");

    println!("\ndemo-incident: holding for {hold}s so the GUI (or the verify script) can look.");
    tokio::time::sleep(Duration::from_secs(hold)).await;
    println!("demo-incident: done.");
}

/// A self-report, as a sensor publishes one.
fn self_report(host_id: &str, source: &str, sensor: &str) -> HostEvidence {
    HostEvidence {
        sensor: sensor.into(),
        source: source.into(),
        observer: None,
        host_id: Some(host_id.into()),
        boot_id: None,
        hostname: Some(source.into()),
        fqdn: None,
        ips: Vec::new(),
        macs: Vec::new(),
        container_id: None,
        cloud: None,
        vendor: None,
        platform: Some("linux".into()),
        last_updated: current_timestamp_millis(),
    }
}

/// The guest's firing alert — an ordinary sensor alert, deliberately not one
/// that mentions the hypervisor. The attribution has to come from the graph,
/// not from the alert's own text, or the demo proves nothing.
fn guest_alert() -> Alert {
    Alert::new(
        "vm101",
        Protocol::Sysinfo,
        AlertKind::Expectation,
        "disk-full",
        AlertSeverity::Critical,
        "/ is 96% full on vm101",
    )
}

fn ctx(origin: &str, producer: &str) -> V1Context {
    V1Context::with_origin(
        Origin::Host(HostId::parse(origin).expect("a legal host origin")),
        producer,
    )
    .expect("a legal producer name")
}

/// A declared publisher per key, used once. The registry wrappers a sensor uses
/// are not available to an example, but the shape is the same one the R5 guard
/// exists to enforce: publish through a declared publisher, never `session.put`.
async fn put<T: serde::Serialize>(session: &Arc<zenoh::Session>, key: &str, value: &T) {
    let bytes = zensight_common::encode(value, Format::Json).expect("payload serializes");
    let publisher = session
        .declare_publisher(key.to_string())
        .await
        .unwrap_or_else(|e| panic!("declare publisher on {key}: {e}"));
    publisher
        .put(bytes)
        .await
        .unwrap_or_else(|e| panic!("publish {key}: {e}"));
}

async fn token(
    session: &Arc<zenoh::Session>,
    ctx: &V1Context,
) -> zenoh::liveliness::LivelinessToken {
    let key = ctx.alive_key();
    session
        .liveliness()
        .declare_token(key.as_keyexpr())
        .await
        .unwrap_or_else(|e| panic!("declare liveliness on {key}: {e}"))
}

async fn open(connect: &str) -> zenoh::Session {
    let mut config = zenoh::Config::default();
    // A client, and scouting fully off: a demo must talk to the bus it was
    // pointed at and never to whatever else is on the LAN. Same reasoning as
    // `zensight-common`'s `rpc_get`.
    config.insert_json5("mode", "\"client\"").unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
        .insert_json5("connect/endpoints", &format!("[\"{connect}\"]"))
        .unwrap();
    match zenoh::open(config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "demo-incident: cannot reach a bus at {connect}: {e}\n\
                 Start one first — `just demo-incident` does it for you."
            );
            std::process::exit(2);
        }
    }
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
