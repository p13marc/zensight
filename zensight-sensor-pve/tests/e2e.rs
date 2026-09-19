//! End-to-end: a real HTTP server speaking real `/api2/json`, a real Zenoh
//! peer, and the real poller (#818).
//!
//! The alternative — mocking the client — proves the mock. This stands a
//! fake PVE API up on a loopback port with the exact document shapes Proxmox
//! returns (including its inconsistencies: numbers arriving as strings, flags
//! omitted rather than zeroed, an endpoint that 403s for a `PVEAuditor`
//! token), points the sensor at it, and asserts on what reaches the bus.
//!
//! The four contracts:
//!
//! 1. the 2026-08-28 audit's findings arrive as alerts with the failing
//!    clause in the labels;
//! 2. the state documents carry the configuration facts, and the gauges the
//!    numbers;
//! 3. fixing the hypervisor resolves the alerts (the per-rule reconcile);
//! 4. an endpoint a read-only token cannot see does not make the sensor
//!    unhealthy.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::{Router, extract::State, routing::get};
use serde_json::{Value, json};

use zensight_common::pve::{PveClusterHealth, PveGuest, PveStoragePool};
use zensight_common::relation::{RelationKind, RelationshipEvidence};
use zensight_common::{Alert, AlertState, TelemetryPoint, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

use zensight_sensor_pve::api::PveClient;
use zensight_sensor_pve::config::{PveAlertsConfig, PveConfig};
use zensight_sensor_pve::poller::Poller;

/// Flips when the test "fixes" the hypervisor, so one server can serve both
/// the broken and the repaired configuration.
#[derive(Clone)]
struct Fixture {
    fixed: Arc<AtomicBool>,
}

fn isolated_config() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    // The state documents ride AdvancedPublishers, which refuse to exist
    // without timestamping. `zensight_common::session` forces it on for every
    // real sensor; a test session that omits it silently loses every state
    // document while the telemetry keeps flowing — which is exactly what this
    // test caught the first time it ran.
    c.insert_json5("timestamping/enabled", "true").unwrap();
    c
}

/// `/cluster/resources`, with Proxmox's real quirks: `vmid` as a number,
/// `template` omitted when false, a storage row that reports capacity through
/// `maxdisk`/`disk`.
async fn resources(State(_): State<Fixture>) -> axum::Json<Value> {
    axum::Json(json!({"data": [
        {"type": "qemu", "vmid": 140, "name": "vm-apps", "node": "pve",
         "status": "running", "uptime": 86400, "cpu": 0.05,
         "mem": 1073741824u64, "maxmem": 2147483648u64,
         "disk": 0, "maxdisk": 34359738368u64,
         // These four are in every real row and were parsed away for two
         // releases (#1141).
         "netin": 51200u64, "netout": 12800u64,
         "diskread": 204800u64, "diskwrite": 409600u64},
        {"type": "lxc", "vmid": 201, "name": "ct-registry", "node": "pve",
         "status": "running", "uptime": 3600},
        {"type": "qemu", "vmid": 9000, "name": "tpl-debian", "node": "pve",
         "status": "stopped", "template": 1},
        {"type": "storage", "storage": "local-lvm", "node": "pve",
         "plugintype": "lvmthin", "status": "available", "shared": 0,
         "content": "images,rootdir",
         "maxdisk": 1006632960000u64, "disk": 300000000000u64},
        {"type": "storage", "storage": "backups", "node": "pve",
         "plugintype": "dir", "status": "available", "shared": 0,
         "content": "backup",
         "maxdisk": 4000000000000u64, "disk": 100000000000u64},
    ]}))
}

async fn guest_config(
    State(f): State<Fixture>,
    // Three path params — node, kind, vmid — and all three must be named or
    // axum silently hands the wrong one over.
    axum::extract::Path((_node, _kind, vmid)): axum::extract::Path<(String, String, u32)>,
) -> axum::Json<Value> {
    let fixed = f.fixed.load(Ordering::Relaxed);
    let data = match vmid {
        // VM 140 as the audit found it: no `onboot` key at all (which means
        // 0) and a NIC line with no `firewall=` (which means the .fw file is
        // inert). Both facts are ABSENCES, which is exactly why a strict
        // deserializer would have missed them.
        140 if !fixed => json!({
            "name": "vm-apps",
            "net0": "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,tag=30",
            "scsi0": "local-lvm:vm-140-disk-0,size=600G",
            "ide2": "local:iso/debian.iso,media=cdrom",
        }),
        140 => json!({
            "name": "vm-apps",
            "onboot": 1,
            "net0": "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,tag=30,firewall=1",
            "scsi0": "local-lvm:vm-140-disk-0,size=600G",
        }),
        201 => json!({
            "hostname": "ct-registry",
            "onboot": 1,
            "net0": "name=eth0,bridge=vmbr0,firewall=1,hwaddr=BC:24:11:00:00:01,ip=dhcp",
            "rootfs": "local-lvm:subvol-201-disk-0,size=8G",
            "mp0": "local-lvm:subvol-201-disk-1,mp=/data,size=350G,backup=0",
        }),
        _ => json!({"template": 1}),
    };
    axum::Json(json!({ "data": data }))
}

/// Volumes on the pool. 600 G + 8 G + 350 G = 958 G promised against 937 G of
/// capacity — the audit's over-commitment, with `used` showing 30 %.
async fn storage_content(
    State(_): State<Fixture>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::Json<Value> {
    if q.get("content").map(String::as_str) == Some("backup") {
        let (now, day) = (now(), 86400);
        return axum::Json(json!({"data": [
            {"volid": "local:backup/vzdump-qemu-140-2026_08_28-02_00_01.vma.zst",
             "size": 5000000000u64, "ctime": now - day, "format": "vma.zst"},
            {"volid": "local:backup/vzdump-qemu-140-2026_08_27-02_00_01.vma.zst",
             "size": 10000000000u64, "ctime": now - 2 * day, "format": "vma.zst"},
        ]}));
    }
    axum::Json(json!({"data": [
        // `size` as a STRING, which Proxmox really does on some plugins.
        {"volid": "local-lvm:vm-140-disk-0", "size": "644245094400", "content": "images"},
        {"volid": "local-lvm:subvol-201-disk-0", "size": 8589934592u64, "content": "rootdir"},
        {"volid": "local-lvm:subvol-201-disk-1", "size": 375809638400u64, "content": "rootdir"},
    ]}))
}

/// Wall clock, so the fixture's ages are ages and not fixed epochs — the
/// staleness bound (#880) is a real rule and the test must feel it.
fn now() -> i64 {
    zensight_common::current_timestamp_millis() / 1000
}

/// The reference fleet's task list, with the three shapes that produced
/// permanently-firing false criticals (#880).
async fn tasks(State(_): State<Fixture>) -> axum::Json<Value> {
    let (now, hour, day) = (now(), 3600, 86400);
    axum::Json(json!({"data": [
        {"upid": "UPID:pve:0001:vzdump:140:", "id": "140", "type": "vzdump",
         "starttime": now - 5 * hour, "endtime": now - 5 * hour + 240,
         "exitstatus": "OK", "node": "pve", "status": "stopped"},
        // Still running: no verdict yet, and must not count as a failure.
        {"upid": "UPID:pve:0002:vzdump:201:", "id": "201", "type": "vzdump",
         "starttime": now - 300, "node": "pve", "status": "running"},
        // A WHOLE-JOB run (`all 1`): PVE gives it an EMPTY id, because it
        // covers every guest and names none. This row used to be dropped
        // outright, which is what sent the sensor looking for a per-guest
        // task and finding the July one below.
        {"upid": "UPID:pve:0003:vzdump::", "id": "", "type": "vzdump",
         "starttime": now - 6 * hour, "endtime": now - 6 * hour + 1800,
         "exitstatus": "job errors", "node": "pve", "status": "stopped"},
        // A one-off from six weeks ago that failed. It is the newest task
        // TAGGED with 201, and it is not evidence about last night.
        {"upid": "UPID:pve:0004:vzdump:201:", "id": "201", "type": "vzdump",
         "starttime": now - 42 * day, "endtime": now - 42 * day + 14,
         "exitstatus": "command failed", "node": "pve", "status": "stopped"},
        // The same, for a TEMPLATE the job excludes. It fired a critical.
        {"upid": "UPID:pve:0005:vzdump:9000:", "id": "9000", "type": "vzdump",
         "starttime": now - 2 * hour, "endtime": now - 2 * hour + 9,
         "exitstatus": "command failed", "node": "pve", "status": "stopped"},
    ]}))
}

async fn nodes(State(_): State<Fixture>) -> axum::Json<Value> {
    axum::Json(json!({"data": [{"node": "pve", "status": "online"}]}))
}

/// `/nodes/{node}/status` (#1141), with the shapes PVE really serves: a
/// `loadavg` array of **strings**, memory and rootfs as nested objects, and
/// the CPU count behind `cpuinfo`.
///
/// The node is deliberately unhealthy — a nearly full `/`, load well past four
/// per CPU, and swap in use — because the three rules this endpoint exists for
/// are what make it worth reading.
async fn node_status(State(f): State<Fixture>) -> axum::Json<Value> {
    if f.fixed.load(Ordering::Relaxed) {
        return axum::Json(json!({"data": {
            "uptime": 864000,
            "cpu": 0.12,
            "cpuinfo": {"cpus": 8},
            "memory": {"total": 68719476736u64, "used": 20000000000u64},
            "swap": {"total": 8589934592u64, "used": 0},
            "rootfs": {"total": 100000000000u64, "used": 20000000000u64},
            "loadavg": ["1.20", "1.10", "0.90"],
            "pveversion": "pve-manager/8.2.4",
            "current-kernel": {"release": "6.8.12-1-pve"},
        }}));
    }
    axum::Json(json!({"data": {
        "uptime": 864000,
        "cpu": 0.97,
        "cpuinfo": {"cpus": 8},
        "memory": {"total": 68719476736u64, "used": 67000000000u64},
        // Swapping: the reading a guest's own numbers cannot show.
        "swap": {"total": 8589934592u64, "used": 8000000000u64},
        // A nearly full `/`. No storage pool's numbers contain this.
        "rootfs": {"total": 100000000000u64, "used": 97000000000u64},
        // Strings, which `as_f64` reads as None — parsing them is the only
        // thing that works here, not belt and braces.
        "loadavg": ["48.50", "44.20", "40.10"],
        "pveversion": "pve-manager/8.2.4",
        "current-kernel": {"release": "6.8.12-1-pve"},
    }}))
}

/// `/cluster/backup` (#1141): one enabled job, due in the past, plus a
/// **disabled** one that must never be called overdue.
async fn backup_jobs(State(_): State<Fixture>) -> axum::Json<Value> {
    axum::Json(json!({"data": [
        {"id": "backup-0001", "enabled": 1, "schedule": "mon..fri 03:00",
         "next-run": 1000, "storage": "backups", "all": 1,
         "comment": "nightly"},
        // Switched off. Its last run is ancient and that is not a fault —
        // it is a job an operator turned off, which is a different thing to
        // say, and the reason `enabled` is carried at all.
        {"id": "backup-0002", "enabled": 0, "schedule": "sat 02:00",
         "next-run": 1000, "storage": "backups", "vmid": "140,201"},
    ]}))
}

/// `/cluster/ceph/status` (#1141). The fixture serves Ceph's real nesting —
/// `health.status`, `health.checks` as an OBJECT keyed by check name, the
/// osdmap double-nested the way older releases serve it, and `pgs_by_state`.
async fn ceph_status(State(f): State<Fixture>) -> axum::Json<Value> {
    if f.fixed.load(Ordering::Relaxed) {
        return axum::Json(json!({"data": {
            "health": {"status": "HEALTH_OK", "checks": {}},
            "osdmap": {"osdmap": {"num_osds": 6, "num_up_osds": 6, "num_in_osds": 6}},
            "monmap": {"mons": [{}, {}, {}]},
            "quorum": [0, 1, 2],
            "pgmap": {"num_pgs": 129, "bytes_used": 500, "bytes_total": 1000,
                      "pgs_by_state": [{"state_name": "active+clean", "count": 129}]},
        }}));
    }
    axum::Json(json!({"data": {
        "health": {"status": "HEALTH_WARN", "checks": {"OSD_DOWN": {}, "PG_DEGRADED": {}}},
        "osdmap": {"osdmap": {"num_osds": 6, "num_up_osds": 5, "num_in_osds": 6}},
        "monmap": {"mons": [{}, {}, {}]},
        "quorum": [0, 1, 2],
        "pgmap": {"num_pgs": 129, "bytes_used": 900, "bytes_total": 1000,
                  "pgs_by_state": [
                      {"state_name": "active+clean", "count": 120},
                      {"state_name": "active+undersized+degraded", "count": 9},
                  ]},
    }}))
}

/// A standalone node: `/cluster/status` answers with no `cluster` row, so
/// `quorate` is unknown rather than false.
async fn cluster_status(State(_): State<Fixture>) -> axum::Json<Value> {
    axum::Json(json!({"data": [
        {"type": "node", "name": "pve", "online": 1, "local": 1, "ip": "10.0.0.2"},
    ]}))
}

/// What a `PVEAuditor` token really gets on an install without HA.
async fn forbidden() -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::FORBIDDEN, "Permission check failed")
}

async fn spawn_api(fixture: Fixture) -> SocketAddr {
    let app = Router::new()
        .route("/api2/json/cluster/resources", get(resources))
        .route("/api2/json/cluster/status", get(cluster_status))
        .route("/api2/json/cluster/ha/status/current", get(forbidden))
        .route("/api2/json/nodes", get(nodes))
        .route("/api2/json/nodes/{node}/replication", get(forbidden))
        .route(
            "/api2/json/nodes/{node}/{kind}/{vmid}/config",
            get(guest_config),
        )
        .route(
            "/api2/json/nodes/{node}/storage/{storage}/content",
            get(storage_content),
        )
        .route("/api2/json/nodes/{node}/tasks", get(tasks))
        .route("/api2/json/nodes/{node}/status", get(node_status))
        .route("/api2/json/cluster/backup", get(backup_jobs))
        .route("/api2/json/cluster/ceph/status", get(ceph_status))
        .with_state(fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn cfg(addr: SocketAddr) -> PveConfig {
    PveConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        token: "PVEAPIToken=test@pve!ro=deadbeef".into(),
        nodes: vec![],
        source: Some("pve".into()),
        poll_interval_secs: 60,
        config_interval_secs: 300,
        backup_interval_secs: 900,
        timeout_secs: 5,
        max_concurrent: 4,
        accept_invalid_certs: false,
        evidence: true,
        alerts: PveAlertsConfig {
            for_secs: 0,
            ..Default::default()
        },
    }
}

/// The fake API speaks plain HTTP; the client builds an `https://` base, so
/// the test overrides it. (Standing a TLS listener up would test rustls, not
/// this sensor.)
fn client(addr: SocketAddr) -> Arc<PveClient> {
    Arc::new(
        PveClient::new(
            format!("http://{addr}/api2/json"),
            "PVEAPIToken=test@pve!ro=deadbeef".into(),
            Duration::from_secs(5),
            false,
            4,
        )
        .unwrap(),
    )
}

async fn recv<T: serde::de::DeserializeOwned>(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    what: &str,
) -> (String, zenoh::sample::SampleKind, Option<T>) {
    let s = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .unwrap_or_else(|_| panic!("{what} timed out"))
        .unwrap_or_else(|_| panic!("{what} channel closed"));
    let key = s.key_expr().to_string();
    let kind = s.kind();
    let body = (kind == zenoh::sample::SampleKind::Put)
        .then(|| decode_auto::<T>(&s.payload().to_bytes()).expect("payload decodes"));
    (key, kind, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hypervisor_contract_end_to_end() {
    let fixture = Fixture {
        fixed: Arc::new(AtomicBool::new(false)),
    };
    let addr = spawn_api(fixture.clone()).await;

    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let alerts_sub = session
        .declare_subscriber("v1/*/state/pve/alert/*")
        .await
        .unwrap();
    let guests_sub = session
        .declare_subscriber("v1/*/state/pve/guest/*")
        .await
        .unwrap();
    let pools_sub = session
        .declare_subscriber("v1/*/state/pve/storage/*")
        .await
        .unwrap();
    let cluster_sub = session
        .declare_subscriber("v1/*/state/pve/cluster")
        .await
        .unwrap();
    let telemetry_sub = session
        .declare_subscriber("v1/*/telemetry/pve/**")
        .await
        .unwrap();
    let relations_sub = session
        .declare_subscriber("v1/*/state/pve/evidence/relation/*")
        .await
        .unwrap();

    let format = zensight_common::Format::Json;
    let publisher = Publisher::new(session.clone(), "pve", format);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Pve,
        format,
    ));
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("pve").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_pve::poller::STATE_QOS),
    );
    let health = Arc::new(zensight_sensor_core::SensorHealth::new("pve"));

    let mut poller = Poller::new(
        client(addr),
        cfg(addr),
        "pve".to_string(),
        publisher,
        states,
        None,
        Some(reporter.clone()),
        health.clone(),
        zensight_sensor_core::relation::RelationSet::new("pve", session.clone(), format),
    );

    // ── Contract 1: the audit's findings arrive as alerts ───────────────────
    let sweep = poller.sweep().await.expect("first sweep");

    // #880, the cadence bug: `sweep()` used to TAKE the backup cache while
    // refilling it only every `backup_interval_secs`, so with the shipped
    // 60 s / 900 s cadences fourteen sweeps in fifteen carried no backups at
    // all — no document published, no backup rule graded, and `reconcile`
    // reading that as "the condition cleared". Every backup alert resolved
    // and re-fired on a 15-minute cycle. Two sweeps of the SAME poller is all
    // it takes to see it; the test below swept two different ones.
    let again = poller.sweep().await.expect("second sweep, same poller");
    assert_eq!(
        again.backups.len(),
        sweep.backups.len(),
        "an off-cadence sweep must carry the cached backups, not an empty vec"
    );
    assert_eq!(again.backup_jobs.len(), sweep.backup_jobs.len());
    assert_eq!(sweep.guests.len(), 3, "two guests and a template");
    poller.publish(&sweep).await;

    // Five assertions fire from this fixture: VM 140's onboot and its NIC, the
    // over-committed pool, the backup that succeeded while halving, and the
    // whole-job vzdump that failed. NOT firing is half the point — see the
    // #880 block below.
    let mut fired: std::collections::HashMap<String, Alert> = Default::default();
    for _ in 0..5 {
        let (_, kind, alert) = recv::<Alert>(&alerts_sub, "alert").await;
        assert_eq!(kind, zenoh::sample::SampleKind::Put);
        let a = alert.unwrap();
        fired.insert(a.rule.clone(), a);
    }
    let onboot = fired
        .get("guest-onboot-off")
        .expect("VM 140's onboot=0 must fire");
    assert_eq!(onboot.labels["vmid"], "140");
    assert_eq!(onboot.labels["name"], "vm-apps");
    assert_eq!(
        onboot.source, "pve",
        "the reporting hypervisor is the source; the vmid is a label (#883)"
    );

    let fw = fired
        .get("guest-nic-firewall-off")
        .expect("VM 140's inert firewall file must fire");
    assert_eq!(fw.labels["nic"], "net0");

    // #880: the three shapes that produced permanently-firing false criticals.
    let job = fired
        .get("backup-job-failed")
        .expect("a failed whole-job vzdump must fire — once");
    assert_eq!(job.labels["node"], "pve");
    assert_eq!(job.labels["exit_status"], "job errors");
    let per_guest_failures: Vec<&str> = fired
        .get("backup-failed")
        .map(|a| vec![a.labels["vmid"].as_str()])
        .unwrap_or_default();
    assert!(
        !per_guest_failures.contains(&"201"),
        "a six-week-old one-off is not evidence about last night"
    );
    assert!(
        !per_guest_failures.contains(&"9000"),
        "a template is excluded from the job and must not be graded on it"
    );

    // #880(b): a volume count is a measurement or it is `None`. Reporting a
    // confident `0` for a listing that was refused, failed or never attempted
    // is the same "silence is not evidence" mistake `HealthState::NeverRan`
    // exists to avoid.
    let b140 = sweep
        .backups
        .iter()
        .find(|b| b.vmid == 140)
        .expect("guest 140 has stored backups");
    assert_eq!(b140.volumes, Some(2));

    let over = fired
        .get("pool-overcommitted")
        .expect("958 G promised on 937 G must fire");
    assert_eq!(over.labels["storage"], "local-lvm");
    assert_eq!(
        over.source, "pve",
        "the reporting hypervisor is the source; the pool is a label (#883)"
    );
    assert!(
        over.summary.contains("no configuration change"),
        "{}",
        over.summary
    );

    // The container is healthy and must NOT have fired: onboot=1, firewall=1.
    assert!(
        !fired
            .values()
            .any(|a| a.labels.get("vmid") == Some(&"201".to_string())),
        "the healthy container fired: {fired:#?}"
    );

    // ── Contract 2: the documents and the gauges ────────────────────────────
    let mut guests: std::collections::HashMap<u32, PveGuest> = Default::default();
    for _ in 0..3 {
        let (_, _, g) = recv::<PveGuest>(&guests_sub, "guest doc").await;
        let g = g.unwrap();
        guests.insert(g.vmid, g);
    }
    let g140 = &guests[&140];
    assert!(!g140.onboot);
    assert_eq!(g140.nics_without_firewall(), vec!["net0"]);
    assert_eq!(
        g140.nics[0].mac.as_deref(),
        Some("AA:BB:CC:DD:EE:FF"),
        "the MAC is the merge evidence"
    );
    assert_eq!(g140.disks.len(), 1, "the CD-ROM is not provisioned storage");
    let g201 = &guests[&201];
    assert_eq!(
        g201.nics[0].mac.as_deref(),
        Some("BC:24:11:00:00:01"),
        "an LXC line carries its MAC in hwaddr, not positionally"
    );
    assert_eq!(g201.disks_excluded_from_backup(), vec!["mp0"]);

    // ── The relationship graph (#916) ───────────────────────────────────────
    //
    // Three guests, three `Hosts` claims, on the registered key family — and
    // the far end has to be *resolvable*, which is what the MAC is for. A
    // claim whose `to` is a bare vmid produces an edge to a node the catalog
    // can never join to the guest's own sensor, which looks on the map like
    // two unrelated machines.
    let mut relations: std::collections::HashMap<u32, RelationshipEvidence> = Default::default();
    for _ in 0..3 {
        let (key, _, r) = recv::<RelationshipEvidence>(&relations_sub, "relation claim").await;
        let r = r.unwrap();
        assert!(
            key.contains("/state/pve/evidence/relation/"),
            "relation claims ride the registered family: {key}"
        );
        // The key chunk is the payload's own derived id — that identity is
        // what makes a refresh an LWW overwrite instead of a new document.
        assert!(
            key.ends_with(&r.relation_id()),
            "key chunk must be the derived relation_id: {key} vs {}",
            r.relation_id()
        );
        assert_eq!(r.kind, RelationKind::Hosts);
        // `from` is a self-claim by host_id: the strongest end available, and
        // what lets the catalog resolve this to a real entity.
        assert!(
            r.from.host_id.is_some(),
            "the node end must be a self-claim: {:?}",
            r.from
        );
        let vmid: u32 = r.to.device.as_deref().unwrap().parse().unwrap();
        relations.insert(vmid, r);
    }
    let r140 = &relations[&140];
    assert_eq!(
        r140.to.macs,
        vec!["AA:BB:CC:DD:EE:FF".to_string()],
        "the guest end carries the MAC, which is the only thing that lets the \
         catalog join the hypervisor's view of this guest to the guest's own"
    );
    assert_eq!(
        r140.attrs.get("bridge").map(String::as_str),
        Some("vmbr1"),
        "the NIC's bridge rides as an attr"
    );
    assert_eq!(
        r140.attrs.get("vlan").map(String::as_str),
        Some("30"),
        "and its VLAN tag, which is what makes two guests on one bridge \
         distinguishable on the map"
    );
    assert!(
        relations.contains_key(&201),
        "an LXC guest is hosted too: {:?}",
        relations.keys().collect::<Vec<_>>()
    );

    let mut pools: std::collections::HashMap<String, PveStoragePool> = Default::default();
    for _ in 0..2 {
        let (_, _, p) = recv::<PveStoragePool>(&pools_sub, "pool doc").await;
        let p = p.unwrap();
        pools.insert(p.storage.clone(), p);
    }
    let pool = &pools["local-lvm"];
    assert!(
        pool.allocated_bytes.unwrap() > pool.total_bytes,
        "{:?} promised vs {} capacity",
        pool.allocated_bytes,
        pool.total_bytes
    );
    assert!(pool.used_ratio() < 0.35, "and `used` shows nothing wrong");

    // ── Contract 4: a 403 is a fact, not a fault ────────────────────────────
    let (_, _, cluster) = recv::<PveClusterHealth>(&cluster_sub, "cluster doc").await;
    let cluster = cluster.unwrap();
    assert_eq!(
        cluster.quorate, None,
        "a standalone node has no quorum to lose"
    );
    assert!(cluster.ha.is_empty(), "HA 403s for a read-only token");
    assert_eq!(cluster.guests_total, 2, "templates are not guests");
    assert_eq!(cluster.guests_running, 2);
    assert_eq!(
        health.snapshot().status,
        zensight_common::HealthStatus::Healthy,
        "a forbidden endpoint must not grade the host unhealthy"
    );

    // Gauges: the backup that succeeded and halved is a number, not only an
    // alert, so a dashboard can see the trend before the threshold trips.
    let mut seen: std::collections::HashMap<String, f64> = Default::default();
    let mut points: Vec<(String, TelemetryPoint)> = Vec::new();
    while let Ok(Ok(sample)) =
        tokio::time::timeout(Duration::from_millis(200), telemetry_sub.recv_async()).await
    {
        if let Ok(p) = decode_auto::<TelemetryPoint>(&sample.payload().to_bytes()) {
            let key = sample.key_expr().as_str();
            let subject = key.split("/telemetry/pve/").nth(1).unwrap().to_string();
            if let zensight_common::TelemetryValue::Gauge(v) = p.value {
                seen.insert(subject.clone(), v);
            }
            points.push((subject, p));
        }
    }

    // #883: every point is filed under the REPORTING HOST. The gap that let
    // this ship was that these assertions keyed only off the key expression,
    // so a `source` naming the guest, the pool or the probe target passed
    // unnoticed — and the GUI groups host cards by `(protocol, source)`, so
    // none of it landed on a card. Each subject stays in the key and, from
    // here on, in the labels.
    assert!(!points.is_empty());
    for (subject, p) in &points {
        assert_eq!(
            p.source, "pve",
            "{subject} is filed under {} rather than the reporting host",
            p.source
        );
    }
    let by_subject = |s: &str| {
        points
            .iter()
            .find(|(k, _)| k == s)
            .unwrap_or_else(|| panic!("no point for {s}"))
            .1
            .clone()
    };
    assert_eq!(by_subject("guest/140/running").labels["vmid"], "140");
    assert_eq!(by_subject("guest/140/running").labels["name"], "vm-apps");
    // `pve-local-lvm`, not `local-lvm`: the pool is `shared: 0`, and a
    // non-shared pool carries its node in the chunk unconditionally (#1132).
    // It used to depend on whether THIS SWEEP saw the name twice, so the key
    // of a surviving node's `local-lvm` moved the moment another node dropped
    // out of the cluster — and its old state document became an LWW ghost.
    assert_eq!(
        by_subject("storage/pve-local-lvm/used_ratio").labels["storage"],
        "local-lvm"
    );
    assert!(
        !points
            .iter()
            .any(|(k, _)| k.starts_with("storage/local-lvm/")),
        "a non-shared pool must not publish under the bare name, whatever \
         else this sweep happened to see"
    );
    assert_eq!(
        by_subject("backup/140/size_change_pct").labels["vmid"],
        "140",
        "the backup points named no subject at all before #883"
    );
    assert_eq!(seen.get("guest/140/running"), Some(&1.0));
    // The runtime numbers the registry advertises must actually be emitted —
    // a registered family with no emitter is a promise `introspect` makes and
    // nobody keeps.
    assert_eq!(seen.get("guest/140/cpu_ratio"), Some(&0.05));
    assert_eq!(seen.get("guest/140/mem_bytes"), Some(&1073741824.0));
    assert!(
        !seen.contains_key("guest/201/cpu_ratio"),
        "the container reported no cpu; a 0 would read as idle"
    );
    assert!(
        seen.get("storage/pve-local-lvm/overcommit_ratio")
            .is_some_and(|r| *r > 1.0),
        "{seen:#?}"
    );
    // The trend is a number before it is an alert, so a dashboard can see a
    // backup shrinking before any threshold trips.
    assert_eq!(
        seen.get("backup/140/size_change_pct"),
        Some(&-50.0),
        "{seen:#?}"
    );
    assert_eq!(seen.get("backup/140/ok"), Some(&1.0), "the task exited OK");

    // ── Contract 3: fixing the hypervisor resolves the alerts ───────────────
    fixture.fixed.store(true, Ordering::Relaxed);
    // Force the config cadence to re-read by building a fresh poller — the
    // slow cadence is the point of the design, not something to defeat here.
    let mut poller2 = Poller::new(
        client(addr),
        cfg(addr),
        "pve".to_string(),
        Publisher::new(session.clone(), "pve", format),
        Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                session.clone(),
                zensight_sensor_core::v1::for_producer("pve").telemetry_prefix(),
                format,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_sensor_pve::poller::STATE_QOS),
        ),
        None,
        Some(reporter.clone()),
        health.clone(),
        zensight_sensor_core::relation::RelationSet::new("pve", session.clone(), format),
    );
    let sweep2 = poller2.sweep().await.expect("second sweep");
    let g140 = sweep2.guests.iter().find(|g| g.vmid == 140).unwrap();
    assert!(g140.onboot, "the fixture repaired VM 140");
    poller2.publish(&sweep2).await;

    // The two guest alerts clear; the over-commitment and the shrunk backup do
    // NOT, because nothing about the pool or the dumps changed. A sweep that
    // resolved everything would be exactly the bug the per-rule reconcile
    // exists to prevent.
    let mut resolved = std::collections::HashSet::new();
    while let Ok(Ok(sample)) =
        tokio::time::timeout(Duration::from_millis(300), alerts_sub.recv_async()).await
    {
        if sample.kind() == zenoh::sample::SampleKind::Put
            && let Ok(a) = decode_auto::<Alert>(&sample.payload().to_bytes())
            && a.state == AlertState::Resolved
        {
            resolved.insert(a.rule);
        }
    }
    assert!(
        resolved.contains("guest-onboot-off") && resolved.contains("guest-nic-firewall-off"),
        "the repaired guest's alerts must clear: {resolved:?}"
    );
    assert!(
        !resolved.contains("pool-overcommitted"),
        "nothing about the pool changed"
    );
    assert_eq!(
        reporter.active_count(),
        3,
        "the over-commitment, the shrunk backup and the failed whole-job run \
         must keep firing"
    );
}

// ── #1141: the hypervisor, the schedules, Ceph, and the dropped counters ─────

/// **The sensor sees the hypervisor.**
///
/// It reported every guest and every pool while the node those guests run on
/// was invisible — which is the first thing anyone looks at when a guest is
/// slow. A node swapping, or with a full `/`, or with a load average six times
/// its core count, showed up nowhere; every guest on it merely looked unhappy.
///
/// The fixture serves the shapes PVE really serves, and the one that matters
/// is `loadavg`: an array of **strings**. `as_f64` reads those as `None`, so
/// parsing the string is not belt and braces here — it is the only thing that
/// works, and a client that did not would publish no load at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_node_itself_is_read_and_graded() {
    let fixture = Fixture {
        fixed: Arc::new(AtomicBool::new(false)),
    };
    let addr = spawn_api(fixture.clone()).await;
    let c = client(addr);

    let node = c
        .node_status("pve")
        .await
        .expect("node status reads")
        .expect("the node answered");

    assert_eq!(node.name, "pve");
    assert_eq!(node.cpus, Some(8));
    assert_eq!(
        node.load1,
        Some(48.5),
        "loadavg arrives as STRINGS and must still be a number"
    );
    assert_eq!(node.load5, Some(44.2));
    assert_eq!(node.load_per_cpu(), Some(48.5 / 8.0));
    assert_eq!(node.pve_version.as_deref(), Some("pve-manager/8.2.4"));
    assert_eq!(node.kernel.as_deref(), Some("6.8.12-1-pve"));
    assert_eq!(node.rootfs_ratio(), Some(0.97));
    assert_eq!(node.swap_ratio().map(|r| (r * 100.0).round()), Some(93.0));

    let nodes = [node];
    let firing = zensight_sensor_pve::alerts::grade(
        &PveAlertsConfig {
            for_secs: 0,
            ..Default::default()
        },
        &zensight_sensor_pve::alerts::Observation {
            source: "pve",
            guests: &[],
            pools: &[],
            backups: &[],
            backup_jobs: &[],
            cluster: None,
            nodes: &nodes,
            schedules: &[],
            ceph: None,
            now_ms: 0,
        },
    );
    let mut rules: Vec<&str> = firing.iter().map(|a| a.rule.as_str()).collect();
    rules.sort_unstable();
    assert_eq!(
        rules,
        vec!["node-load-high", "node-rootfs-full", "node-swapping"],
        "{firing:?}"
    );
    // The root-filesystem one is the point: no storage pool's numbers contain
    // `/`, so `pool-usage` could never have said this.
    let rootfs = firing
        .iter()
        .find(|a| a.rule == "node-rootfs-full")
        .unwrap();
    assert!(rootfs.summary.contains("97%"), "{}", rootfs.summary);
    assert!(
        rootfs.labels.get("node").is_some_and(|v| v == "pve"),
        "{:?}",
        rootfs.labels
    );

    // And a healthy node fires none of them.
    fixture.fixed.store(true, Ordering::Relaxed);
    let healthy = [c.node_status("pve").await.unwrap().unwrap()];
    let quiet = zensight_sensor_pve::alerts::grade(
        &PveAlertsConfig {
            for_secs: 0,
            ..Default::default()
        },
        &zensight_sensor_pve::alerts::Observation {
            source: "pve",
            guests: &[],
            pools: &[],
            backups: &[],
            backup_jobs: &[],
            cluster: None,
            nodes: &healthy,
            schedules: &[],
            ceph: None,
            now_ms: 0,
        },
    );
    assert!(quiet.is_empty(), "{quiet:?}");
}

/// **"Due at 03:00 and did not run" is a different claim from "old".**
///
/// `backup-stale` measures a fixed age, so a job that was switched **off**, or
/// whose schedule was edited away, looks exactly like one that is merely
/// young. The schedule says when it was due.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_backup_job_is_never_called_overdue() {
    let fixture = Fixture {
        fixed: Arc::new(AtomicBool::new(false)),
    };
    let addr = spawn_api(fixture).await;
    let jobs = client(addr).backup_jobs().await.expect("backup jobs read");

    assert_eq!(jobs.len(), 2);
    let on = jobs.iter().find(|j| j.id == "backup-0001").unwrap();
    assert!(on.enabled);
    assert!(on.all_guests, "`all: 1` takes every guest");
    assert_eq!(on.schedule.as_deref(), Some("mon..fri 03:00"));
    assert_eq!(
        on.next_run_ms,
        Some(1_000_000),
        "next-run is seconds on the wire"
    );

    let off = jobs.iter().find(|j| j.id == "backup-0002").unwrap();
    assert!(!off.enabled, "`enabled: 0` is the off switch");
    assert_eq!(off.guests, Some(2), "two vmids named");

    let cfg = PveAlertsConfig {
        for_secs: 0,
        ..Default::default()
    };
    let obs = |now_ms: i64| zensight_sensor_pve::alerts::Observation {
        source: "pve",
        guests: &[],
        pools: &[],
        backups: &[],
        backup_jobs: &[],
        cluster: None,
        nodes: &[],
        schedules: &jobs,
        ceph: None,
        now_ms,
    };

    // Long past both jobs' next-run, with nothing having run.
    let firing = zensight_sensor_pve::alerts::grade(&cfg, &obs(9_999_999_999));
    let overdue: Vec<&Alert> = firing
        .iter()
        .filter(|a| a.rule == "backup-job-overdue")
        .collect();
    assert_eq!(
        overdue.len(),
        1,
        "only the ENABLED job is overdue — a job an operator switched off is \
         not a fault: {firing:?}"
    );
    assert!(
        overdue[0].summary.contains("backup-0001"),
        "{}",
        overdue[0].summary
    );
    assert!(
        overdue[0].summary.contains("nightly"),
        "the operator's own name for it: {}",
        overdue[0].summary
    );
    assert!(
        overdue[0].summary.contains("mon..fri 03:00"),
        "and the schedule it missed: {}",
        overdue[0].summary
    );

    // Inside the grace, nothing fires.
    let inside = zensight_sensor_pve::alerts::grade(&cfg, &obs(1_000_000 + 60_000));
    assert!(
        !inside.iter().any(|a| a.rule == "backup-job-overdue"),
        "a job due at 03:00 that starts at 03:02 is not overdue: {inside:?}"
    );
}

/// Ceph's **own** enum, never our reading of the counters beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ceph_health_is_cephs_verdict_and_absent_where_there_is_no_ceph() {
    let fixture = Fixture {
        fixed: Arc::new(AtomicBool::new(false)),
    };
    let addr = spawn_api(fixture.clone()).await;
    let c = client(addr);

    let ceph = c
        .ceph_status()
        .await
        .expect("ceph status reads")
        .expect("this fixture runs ceph");
    assert_eq!(ceph.health, "HEALTH_WARN");
    let mut checks = ceph.checks.clone();
    checks.sort();
    assert_eq!(
        checks,
        vec!["OSD_DOWN", "PG_DEGRADED"],
        "the names an operator acts on"
    );
    assert_eq!(ceph.osds_up, Some(5));
    assert_eq!(ceph.osds_total, Some(6));
    assert_eq!(
        ceph.pgs_degraded,
        Some(9),
        "summed over every state that is not active+clean — the only reading \
         that survives Ceph adding a state name"
    );
    assert!(ceph.is_faulted());

    let cfg = PveAlertsConfig {
        for_secs: 0,
        ..Default::default()
    };
    let firing = zensight_sensor_pve::alerts::grade(
        &cfg,
        &zensight_sensor_pve::alerts::Observation {
            source: "pve",
            guests: &[],
            pools: &[],
            backups: &[],
            backup_jobs: &[],
            cluster: None,
            nodes: &[],
            schedules: &[],
            ceph: Some(&ceph),
            now_ms: 0,
        },
    );
    let a = firing.iter().find(|a| a.rule == "ceph-health").unwrap();
    assert_eq!(a.severity, zensight_common::AlertSeverity::Warning);
    assert!(a.summary.contains("HEALTH_WARN"), "{}", a.summary);
    assert!(a.summary.contains("OSD_DOWN"), "{}", a.summary);

    // HEALTH_OK is not a fault, even with a PG count that is not round.
    fixture.fixed.store(true, Ordering::Relaxed);
    let ok = c.ceph_status().await.unwrap().unwrap();
    assert!(!ok.is_faulted());
    let quiet = zensight_sensor_pve::alerts::grade(
        &cfg,
        &zensight_sensor_pve::alerts::Observation {
            source: "pve",
            guests: &[],
            pools: &[],
            backups: &[],
            backup_jobs: &[],
            cluster: None,
            nodes: &[],
            schedules: &[],
            ceph: Some(&ok),
            now_ms: 0,
        },
    );
    assert!(!quiet.iter().any(|a| a.rule == "ceph-health"), "{quiet:?}");
}

/// The four counters the `/cluster/resources` row already carried.
///
/// They were parsed away for two releases: the rows have them and this sensor
/// dropped them on the floor, so "which guest is saturating the uplink" was a
/// question the hypervisor could answer and ZenSight could not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guest_counters_in_the_row_are_not_dropped() {
    let fixture = Fixture {
        fixed: Arc::new(AtomicBool::new(false)),
    };
    let addr = spawn_api(fixture).await;
    let (runtimes, _) = client(addr).resources().await.expect("resources read");

    let vm = runtimes.iter().find(|g| g.vmid == 140).unwrap();
    assert_eq!(vm.netin, Some(51200));
    assert_eq!(vm.netout, Some(12800));
    assert_eq!(vm.diskread, Some(204800));
    assert_eq!(vm.diskwrite, Some(409600));

    // A row without them stays absent rather than becoming zero — a container
    // whose row carries no counters has not transferred nothing.
    let ct = runtimes.iter().find(|g| g.vmid == 201).unwrap();
    assert_eq!(ct.netin, None, "a field the row omits is missing, not zero");
}
