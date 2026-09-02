//! End-to-end: a real UNIX-socket HTTP server speaking real libpod JSON, a
//! real cgroup tree on disk, a real Zenoh peer, and the real poller (#819).
//!
//! The fixture is the reference fleet's own failures:
//!
//! - **garage** — healthcheck configured, `unhealthy`, empty log, because a
//!   `CMD-SHELL` probe in a distroless image cannot run. It reported
//!   `unhealthy` from the day it was deployed while serving traffic perfectly,
//!   and nobody noticed for weeks.
//! - **netring** — a cgroup that has been OOM-killed, with a `memory.max` of
//!   64 MiB. On 2026-08-17 five sensors shared one cgroup, so this number did
//!   not exist and the kill was blamed on "the bundle" for eleven days.
//! - **caddy** — healthy, so the sweep has something that must NOT fire.
//! - **oldjob** — exited 137.
//!
//! The HTTP server is hand-rolled rather than pulled in: a UNIX-socket
//! listener speaking one request-response per connection is forty lines, and
//! the point is to exercise the sensor's own client, not a framework's.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use zensight_common::container::{ContainerInfo, HealthState};
use zensight_common::relation::{RelationKind, RelationshipEvidence};
use zensight_common::{Alert, AlertState, TelemetryPoint, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

use zensight_sensor_container::config::{ContainerAlertsConfig, ContainerConfig};
use zensight_sensor_container::poller::Poller;
use zensight_sensor_container::runtime::RuntimeClient;

fn isolated_config() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    // AdvancedPublishers refuse to exist without it, and every state document
    // rides one.
    c.insert_json5("timestamping/enabled", "true").unwrap();
    c
}

fn list_body() -> String {
    serde_json::json!([
        {"Id": "id-caddy",   "Names": ["caddy"]},
        {"Id": "id-garage",  "Names": ["garage"]},
        {"Id": "id-netring", "Names": ["netring"]},
        {"Id": "id-oldjob",  "Names": ["oldjob"]},
    ])
    .to_string()
}

fn inspect_body(id: &str) -> String {
    let v = match id {
        "id-caddy" => serde_json::json!({
            "Id": "id-caddy", "Name": "caddy",
            "Created": "2026-08-01T10:00:00Z",
            "ImageDigest": "sha256:caddy-running",
            "State": {"Status": "running", "StartedAt": "2026-08-01T10:00:05Z",
                      "Health": {"Status": "healthy", "Log": [{"ExitCode": 0}]}},
            "Config": {"Image": "docker.io/library/caddy:2.11.4",
                       "Healthcheck": {"Test": ["CMD", "caddy", "version"]},
                       "Labels": {"PODMAN_SYSTEMD_UNIT": "caddy.service"}},
            "HostConfig": {"RestartPolicy": {"Name": "always"}},
            "NetworkSettings": {"IPAddress": "10.89.0.5",
                                "Ports": {"443/tcp": [{"HostIp": "", "HostPort": "443"}]}},
            "Mounts": [{"Type": "bind", "Source": "/etc/caddy", "Destination": "/etc/caddy", "RW": false}],
            "CgroupPath": "/machine.slice/libpod-id-caddy.scope"
        }),
        // The garage case, verbatim: configured, "unhealthy", EMPTY log.
        "id-garage" => serde_json::json!({
            "Id": "id-garage", "Name": "garage",
            "State": {"Status": "running", "StartedAt": "2026-07-01T00:00:00Z",
                      "Health": {"Status": "unhealthy", "FailingStreak": 999, "Log": []}},
            "Config": {"Image": "docker.io/dxflrs/garage:v1.0.1",
                       "Healthcheck": {"Test": ["CMD-SHELL", "/bin/true"]},
                       "Labels": {"PODMAN_SYSTEMD_UNIT": "garage.service"}},
            "CgroupPath": "/machine.slice/libpod-id-garage.scope"
        }),
        "id-netring" => serde_json::json!({
            "Id": "id-netring", "Name": "netring",
            "State": {"Status": "running", "StartedAt": "2026-08-17T00:00:00Z"},
            "RestartCount": 2,
            "Config": {"Image": "git.marcpardo.eu/marcpardo/zensight-sensor-netring:0.11.0",
                       "Labels": {"PODMAN_SYSTEMD_UNIT": "zensight-sensor-netring.service"}},
            "CgroupPath": "/machine.slice/libpod-id-netring.scope"
        }),
        "id-oldjob" => serde_json::json!({
            "Id": "id-oldjob", "Name": "oldjob",
            "State": {"Status": "exited", "ExitCode": 137},
            "Config": {"Image": "docker.io/library/busybox:1.36"},
        }),
        _ => serde_json::json!({}),
    };
    v.to_string()
}

/// One request, one response, one connection. Enough HTTP for the sensor's
/// client, and no more.
async fn spawn_socket(dir: &Path) -> PathBuf {
    let path = dir.join("podman.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = if path.contains("/containers/json") {
                    list_body()
                } else if let Some(rest) = path.split("/containers/").nth(1) {
                    inspect_body(rest.trim_end_matches("/json"))
                } else {
                    "{}".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    path
}

/// A cgroup tree with the numbers that were missing on 2026-08-17.
fn write_cgroups(root: &Path) {
    for (id, mem, max, oom) in [
        ("id-caddy", "20000000", "268435456", "0"),
        ("id-garage", "50000000", "max", "0"),
        // 64 MiB ceiling, sitting on it, and one kill on the counter.
        ("id-netring", "67000000", "67108864", "1"),
    ] {
        let dir = root.join(format!("machine.slice/libpod-{id}.scope"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("memory.current"), format!("{mem}\n")).unwrap();
        std::fs::write(dir.join("memory.max"), format!("{max}\n")).unwrap();
        std::fs::write(
            dir.join("memory.events"),
            format!("low 0\nhigh 0\nmax 9\noom {oom}\noom_kill {oom}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("cpu.stat"), "usage_usec 5000\nthrottled_usec 3\n").unwrap();
        std::fs::write(dir.join("pids.current"), "9\n").unwrap();
    }
}

fn cfg(cgroup_root: &Path) -> ContainerConfig {
    ContainerConfig {
        cgroup_root: cgroup_root.to_string_lossy().into_owned(),
        poll_interval_secs: 30,
        timeout_secs: 5,
        alerts: ContainerAlertsConfig {
            for_secs: 0,
            // No hold: this test drives three sweeps back to back and wants
            // the kill to fire on the second and resolve on the third. The
            // hold window itself is unit-tested in `poller.rs`.
            oom_hold_secs: 0,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_container_contract_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = spawn_socket(tmp.path()).await;
    let cgroup_root = tmp.path().join("cgroup");
    write_cgroups(&cgroup_root);

    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let alerts_sub = session
        .declare_subscriber("v1/*/state/container/alert/*")
        .await
        .unwrap();
    let docs_sub = session
        .declare_subscriber("v1/*/state/container/container/*")
        .await
        .unwrap();
    let telemetry_sub = session
        .declare_subscriber("v1/*/telemetry/container/**")
        .await
        .unwrap();
    let relations_sub = session
        .declare_subscriber("v1/*/state/container/evidence/relation/*")
        .await
        .unwrap();

    let format = zensight_common::Format::Json;
    let publisher = Publisher::new(session.clone(), "container", format);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Container,
        format,
    ));
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("container").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_container::poller::STATE_QOS),
    );
    let health = Arc::new(zensight_sensor_core::SensorHealth::new("container"));
    let client = Arc::new(RuntimeClient::new(&sock, Duration::from_secs(5), false));

    let mut poller = Poller::new(
        vec![client],
        cfg(&cgroup_root),
        "testhost".to_string(),
        publisher,
        states,
        None,
        Some(reporter.clone()),
        health.clone(),
        None,
        zensight_sensor_core::relation::RelationSet::new("container", session.clone(), format),
    );

    // ── Sweep 1 establishes the baseline; the delta rules cannot fire yet ───
    let first = poller.sweep().await.expect("first sweep");
    assert_eq!(first.len(), 4, "four containers");
    poller.publish(&first).await;

    let mut fired: std::collections::HashMap<String, Alert> = Default::default();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), alerts_sub.recv_async()).await
    {
        if s.kind() == zenoh::sample::SampleKind::Put
            && let Ok(a) = decode_auto::<Alert>(&s.payload().to_bytes())
            && a.state == AlertState::Firing
        {
            fired.insert(a.rule.clone(), a);
        }
    }

    // garage: the probe cannot run. NOT the same alert as a failing service,
    // and the summary has to send the operator to the check, not to garage.
    let never = fired
        .get("container-healthcheck-never-ran")
        .expect("garage's un-runnable healthcheck must fire");
    assert_eq!(never.labels["container"], "garage");
    assert_eq!(
        never.labels["unit"], "garage.service",
        "the systemd join, so the alert names something restartable"
    );
    assert!(never.summary.contains("probe itself"), "{}", never.summary);
    assert!(
        !fired.contains_key("container-unhealthy"),
        "garage must not also be reported as a failing service: {fired:#?}"
    );

    // oldjob exited 137.
    let exited = fired
        .get("container-exited-nonzero")
        .expect("a non-zero exit must fire");
    assert_eq!(exited.labels["container"], "oldjob");
    assert_eq!(exited.labels["exit_code"], "137");

    // The OOM is a DELTA rule and there is no baseline yet — it must not fire
    // on the first sweep, or every sensor restart would page.
    assert!(
        !fired.contains_key("container-oom-killed"),
        "no baseline yet: {fired:#?}"
    );

    // ── The documents ───────────────────────────────────────────────────────
    let mut docs: std::collections::HashMap<String, ContainerInfo> = Default::default();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), docs_sub.recv_async()).await
    {
        if let Ok(c) = decode_auto::<ContainerInfo>(&s.payload().to_bytes()) {
            docs.insert(c.name.clone(), c);
        }
    }
    assert_eq!(docs.len(), 4, "{:?}", docs.keys().collect::<Vec<_>>());

    let garage = &docs["garage"];
    assert_eq!(garage.health, HealthState::NeverRan);
    assert_eq!(garage.unit.as_deref(), Some("garage.service"));

    let caddy = &docs["caddy"];
    assert_eq!(caddy.health, HealthState::Healthy);
    assert_eq!(caddy.image.digest.as_deref(), Some("sha256:caddy-running"));
    assert_eq!(caddy.restart_policy.as_deref(), Some("always"));
    assert_eq!(caddy.ips, vec!["10.89.0.5"]);
    assert_eq!(caddy.ports.len(), 1);
    assert!(caddy.mounts[0].read_only);
    // The kernel's half, joined onto the runtime's.
    assert_eq!(caddy.resources.memory_bytes, Some(20_000_000));
    assert_eq!(caddy.resources.memory_max_bytes, Some(268_435_456));

    // ── The relationship graph (#916) ───────────────────────────────────────
    //
    // Three claims, not four: the exited container is not *run* by this host
    // any more. A `Runs` edge to a stopped container would put it on the map
    // as a live dependency and let impact attribution treat its absence as a
    // symptom of something.
    let mut relations: std::collections::HashMap<String, RelationshipEvidence> = Default::default();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), relations_sub.recv_async()).await
    {
        let key = s.key_expr().to_string();
        let Ok(r) = decode_auto::<RelationshipEvidence>(&s.payload().to_bytes()) else {
            continue;
        };
        assert!(
            key.ends_with(&r.relation_id()),
            "the key chunk is the payload's derived id: {key}"
        );
        assert_eq!(r.kind, RelationKind::Runs);
        assert!(r.from.host_id.is_some(), "the host end is a self-claim");
        relations.insert(r.to.name.clone().unwrap(), r);
    }
    assert_eq!(
        relations.len(),
        3,
        "only running containers: {:?}",
        relations.keys().collect::<Vec<_>>()
    );
    let rc = &relations["caddy"];
    assert_eq!(
        rc.to.ips,
        vec!["10.89.0.5".to_string()],
        "the container end carries its IPs, which is what lets the catalog join \
         netlink's wire-only bridge entity to this container"
    );
    assert_eq!(
        rc.attrs.get("unit").map(String::as_str),
        Some("caddy.service"),
        "the owning unit is an attr on the containment, not a second edge"
    );

    // The number whose absence made 2026-08-17 "the bundle" for eleven days.
    let netring = &docs["netring"];
    assert_eq!(netring.resources.memory_bytes, Some(67_000_000));
    assert_eq!(netring.resources.memory_max_bytes, Some(67_108_864));
    assert_eq!(netring.resources.oom_kills, Some(1));
    assert!(
        netring.memory_ratio().unwrap() > 0.99,
        "sitting on its limit"
    );

    // garage is unlimited: no ceiling, not a ceiling of zero.
    assert_eq!(docs["garage"].resources.memory_max_bytes, None);

    // ── The gauges ──────────────────────────────────────────────────────────
    let mut seen: std::collections::HashMap<String, f64> = Default::default();
    let mut points: Vec<(String, TelemetryPoint)> = Vec::new();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), telemetry_sub.recv_async()).await
    {
        if let Ok(p) = decode_auto::<TelemetryPoint>(&s.payload().to_bytes()) {
            let key = s.key_expr().as_str();
            let subject = key
                .split("/telemetry/container/")
                .nth(1)
                .unwrap()
                .to_string();
            if let zensight_common::TelemetryValue::Gauge(v) = p.value {
                seen.insert(subject.clone(), v);
            }
            points.push((subject, p));
        }
    }

    // #883/#884: every point is filed under the host running the container,
    // and every point names the container it is about. Before this, 307 of
    // 307 points on the reference fleet carried no host identity at all while
    // the same sensor's alerts carried `host.id` — and a container name is
    // unique per host, not globally, so four machines running the same image
    // agreed on `source`, `metric` and every label.
    assert!(!points.is_empty());
    for (subject, p) in &points {
        assert_eq!(
            p.source, "testhost",
            "{subject} is filed under {} rather than the reporting host",
            p.source
        );
    }
    let netring_point = points
        .iter()
        .find(|(k, _)| k == "netring/memory_bytes")
        .expect("netring/memory_bytes")
        .1
        .clone();
    assert_eq!(netring_point.labels["container"], "netring");
    assert!(netring_point.labels.contains_key("image"));
    assert_eq!(seen.get("netring/memory_bytes"), Some(&67_000_000.0));
    assert_eq!(seen.get("netring/oom_kills_total"), Some(&1.0));
    assert_eq!(seen.get("caddy/healthy"), Some(&1.0));
    assert_eq!(seen.get("containers/total"), Some(&4.0));
    assert_eq!(seen.get("containers/running"), Some(&3.0));
    // garage's healthcheck cannot run, so there is no honest 0 or 1 to publish
    // — a 0 here would tell every dashboard the service is down.
    assert!(
        !seen.contains_key("garage/healthy"),
        "a never-run check has no healthy gauge: {seen:#?}"
    );
    assert_eq!(
        seen.get("containers/unhealthy"),
        Some(&0.0),
        "and it is not counted as unhealthy either"
    );

    // ── Sweep 2: a new OOM kill against the baseline ────────────────────────
    std::fs::write(
        cgroup_root.join("machine.slice/libpod-id-netring.scope/memory.events"),
        "low 0\nhigh 0\nmax 12\noom 2\noom_kill 2\n",
    )
    .unwrap();
    let second = poller.sweep().await.expect("second sweep");
    poller.publish(&second).await;

    let mut fired2 = std::collections::HashSet::new();
    let mut resolved2 = std::collections::HashSet::new();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), alerts_sub.recv_async()).await
    {
        if s.kind() == zenoh::sample::SampleKind::Put
            && let Ok(a) = decode_auto::<Alert>(&s.payload().to_bytes())
        {
            match a.state {
                AlertState::Firing => {
                    fired2.insert((a.rule.clone(), a.labels["container"].clone()));
                }
                AlertState::Resolved => {
                    resolved2.insert(a.rule.clone());
                }
            }
        }
    }
    assert!(
        fired2.contains(&("container-oom-killed".to_string(), "netring".to_string())),
        "the new kill must fire, and must NAME netring rather than 'the bundle': {fired2:?}"
    );
    assert!(
        resolved2.is_empty(),
        "nothing was fixed, so nothing resolves: {resolved2:?}"
    );

    // ── Sweep 3: the counter stops moving, so the alert clears ──────────────
    let third = poller.sweep().await.expect("third sweep");
    poller.publish(&third).await;
    let mut resolved3 = std::collections::HashSet::new();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), alerts_sub.recv_async()).await
    {
        if s.kind() == zenoh::sample::SampleKind::Put
            && let Ok(a) = decode_auto::<Alert>(&s.payload().to_bytes())
            && a.state == AlertState::Resolved
        {
            resolved3.insert(a.rule);
        }
    }
    assert!(
        resolved3.contains("container-oom-killed"),
        "a cumulative counter that stopped moving must resolve, not fire forever: {resolved3:?}"
    );
    assert_eq!(
        health.snapshot().status,
        zensight_common::HealthStatus::Healthy
    );
}

/// "No runtime" and "no containers" are different answers. A sensor that
/// renders both as an empty list is lying about one of them, and the lie is
/// the dangerous direction: a fleet view showing zero containers on a host
/// whose podman is dead reads as a quiet host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_runtime_is_an_error_not_an_empty_fleet() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session.clone(), "container", zensight_common::Format::Json);
    let states = Arc::new(zensight_sensor_core::AdvancedPublisherRegistry::new(
        session.clone(),
        zensight_sensor_core::v1::for_producer("container").telemetry_prefix(),
        zensight_common::Format::Json,
        zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
    ));
    let mut poller = Poller::new(
        vec![Arc::new(RuntimeClient::new(
            tmp.path().join("absent.sock"),
            Duration::from_millis(200),
            false,
        ))],
        cfg(tmp.path()),
        "testhost".to_string(),
        publisher,
        states,
        None,
        None,
        Arc::new(zensight_sensor_core::SensorHealth::new("container")),
        None,
        zensight_sensor_core::relation::RelationSet::new(
            "container",
            session.clone(),
            zensight_common::Format::Json,
        ),
    );
    let err = poller.sweep().await.unwrap_err();
    assert!(err.contains("cannot reach"), "{err}");
}
