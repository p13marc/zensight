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
         "disk": 0, "maxdisk": 34359738368u64},
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
    assert_eq!(
        by_subject("storage/local-lvm/used_ratio").labels["storage"],
        "local-lvm"
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
        seen.get("storage/local-lvm/overcommit_ratio")
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
