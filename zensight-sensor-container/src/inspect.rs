//! Turning a runtime's inspect document into a [`ContainerInfo`] (#819).
//!
//! Pure: it takes JSON and returns a document, so every field the audit's
//! findings depend on is testable against a real inspect payload without a
//! socket. The runtime dialects differ in where they put things — podman's
//! `libpod` and Docker's compatibility API disagree on capitalisation, on
//! whether the image digest is present at all, and on how a healthcheck that
//! has never run is represented — so every read here is by name with a
//! fallback, never positional.

use serde_json::Value;

use zensight_common::container::{
    ContainerImage, ContainerInfo, ContainerResources, HealthState, MountPoint, PortBinding,
    SignatureState,
};

/// The label Quadlet sets on every container it manages. It is the join with
/// the systemd sensor's view: without it a container and its unit are two
/// unrelated rows in two different views of the same thing.
pub const SYSTEMD_UNIT_LABEL: &str = "PODMAN_SYSTEMD_UNIT";

/// Build the document from a list entry joined with its inspect reply.
///
/// `list` may be `Null` when only an inspect is available.
pub fn build(list: &Value, inspect: &Value, rootless: bool) -> Option<ContainerInfo> {
    let id = first_str(&[get(inspect, "Id"), get(list, "Id")])?;
    let name = container_name(list, inspect).unwrap_or_else(|| id.chars().take(12).collect());

    let state = inspect.get("State");
    let config = inspect.get("Config");

    let status = first_str(&[
        state.and_then(|s| s.get("Status")),
        get(list, "State"),
        get(inspect, "Status"),
    ])
    .unwrap_or_else(|| "unknown".into())
    .to_ascii_lowercase();

    let labels = config
        .and_then(|c| c.get("Labels"))
        .or_else(|| list.get("Labels"))
        .and_then(Value::as_object);

    let health = health_state(state, config);

    ContainerInfo {
        image: ContainerImage {
            reference: first_str(&[
                config.and_then(|c| c.get("Image")),
                get(inspect, "ImageName"),
                get(list, "Image"),
            ])
            .unwrap_or_else(|| "unknown".into()),
            // The digest actually running. Podman reports it as `ImageDigest`;
            // Docker's `Image` field on the inspect root is the digest while
            // `Config.Image` is the reference — a genuinely confusing pair, and
            // reading the wrong one gives a digest that is really a tag.
            digest: first_str(&[
                get(inspect, "ImageDigest"),
                get(list, "ImageID"),
                get(inspect, "Image"),
            ])
            .filter(|d| d.starts_with("sha256:")),
            upstream_digest: None,
            signature: SignatureState::NotChecked,
        },
        created_at: first_str(&[get(inspect, "Created"), get(list, "Created")])
            .as_deref()
            .and_then(parse_rfc3339)
            .or_else(|| num(list, "Created").map(|n| n as i64)),
        started_at: state
            .and_then(|s| s.get("StartedAt"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
        restart_count: first_num(&[
            get(inspect, "RestartCount"),
            state.and_then(|s| s.get("Restarts")),
            get(list, "Restarts"),
        ])
        .unwrap_or(0.0) as u64,
        // Only meaningful once it has stopped. A running container's
        // `ExitCode` is 0 and reporting it would say "exited cleanly".
        exit_code: (status != "running")
            .then(|| state.and_then(|s| num(s, "ExitCode")).map(|n| n as i64))
            .flatten(),
        health,
        health_failing_streak: state
            .and_then(|s| s.get("Health"))
            .and_then(|h| num(h, "FailingStreak"))
            .map(|n| n as u64),
        unit: labels
            .and_then(|l| l.get(SYSTEMD_UNIT_LABEL))
            .and_then(Value::as_str)
            .map(str::to_string),
        restart_policy: inspect
            .get("HostConfig")
            .and_then(|h| h.get("RestartPolicy"))
            .and_then(|r| r.get("Name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        rootless,
        ports: ports(inspect, list),
        mounts: mounts(inspect),
        cgroup_path: state
            .and_then(|s| s.get("CgroupPath"))
            .or_else(|| inspect.get("CgroupPath"))
            .and_then(Value::as_str)
            .map(str::to_string),
        resources: ContainerResources::default(),
        ips: ips(inspect),
        id,
        name,
        status,
        observed_at_ms: zensight_common::current_timestamp_millis(),
    }
    .into()
}

/// The healthcheck state, including the one distinction the garage case turns
/// on.
///
/// garage reported `unhealthy` from the day it was deployed while serving
/// traffic perfectly: a distroless image with no `/bin/sh`, so its `CMD-SHELL`
/// probe could never execute. "The check cannot run" and "the service is
/// failing" are different facts and had been rendering as the same one.
fn health_state(state: Option<&Value>, config: Option<&Value>) -> HealthState {
    let configured = config
        .and_then(|c| c.get("Healthcheck"))
        .is_some_and(|h| !h.is_null())
        || state
            .and_then(|s| s.get("Health"))
            .is_some_and(|h| !h.is_null());
    if !configured {
        return HealthState::None;
    }
    let health = state.and_then(|s| s.get("Health"));
    let status = health
        .and_then(|h| h.get("Status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let log_len = health
        .and_then(|h| h.get("Log"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    match status {
        "healthy" => HealthState::Healthy,
        "unhealthy" if log_len == 0 => HealthState::NeverRan,
        "unhealthy" => HealthState::Unhealthy,
        "starting" if log_len == 0 => HealthState::NeverRan,
        "starting" => HealthState::Starting,
        // Configured but with no status at all: the runtime has not started
        // the check. Not healthy, and not a failing service either.
        _ => HealthState::NeverRan,
    }
}

fn container_name(list: &Value, inspect: &Value) -> Option<String> {
    // libpod inspect: "Name": "caddy". Docker: "/caddy" — the leading slash is
    // a wire artefact, and leaving it on puts a `/` in every device slug.
    if let Some(n) = get(inspect, "Name").and_then(Value::as_str) {
        return Some(n.trim_start_matches('/').to_string());
    }
    list.get("Names")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|n| n.trim_start_matches('/').to_string())
}

fn ports(inspect: &Value, list: &Value) -> Vec<PortBinding> {
    // libpod inspect: NetworkSettings.Ports is a map
    // "80/tcp": [{"HostIp": "", "HostPort": "8080"}]
    let mut out = Vec::new();
    if let Some(map) = inspect
        .get("NetworkSettings")
        .and_then(|n| n.get("Ports"))
        .and_then(Value::as_object)
    {
        for (spec, bindings) in map {
            let (port, proto) = match spec.split_once('/') {
                Some((p, pr)) => (p.parse().ok(), Some(pr.to_string())),
                None => (spec.parse().ok(), None),
            };
            let Some(container_port) = port else { continue };
            let entries = bindings.as_array().map(Vec::as_slice).unwrap_or(&[]);
            if entries.is_empty() {
                out.push(PortBinding {
                    container_port,
                    host_port: None,
                    host_ip: None,
                    protocol: proto.clone(),
                });
            }
            for b in entries {
                out.push(PortBinding {
                    container_port,
                    host_port: b
                        .get("HostPort")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse().ok()),
                    host_ip: b
                        .get("HostIp")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    protocol: proto.clone(),
                });
            }
        }
    }
    if out.is_empty()
        && let Some(arr) = list.get("Ports").and_then(Value::as_array)
    {
        for p in arr {
            if let Some(container_port) = num(p, "container_port").or_else(|| num(p, "PrivatePort"))
            {
                out.push(PortBinding {
                    container_port: container_port as u16,
                    host_port: num(p, "host_port")
                        .or_else(|| num(p, "PublicPort"))
                        .map(|n| n as u16),
                    host_ip: p
                        .get("host_ip")
                        .or_else(|| p.get("IP"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    protocol: p
                        .get("protocol")
                        .or_else(|| p.get("Type"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
    }
    out.sort_by_key(|p| (p.container_port, p.host_port));
    out.dedup();
    out
}

fn mounts(inspect: &Value) -> Vec<MountPoint> {
    inspect
        .get("Mounts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some(MountPoint {
                        source: m
                            .get("Source")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        destination: m.get("Destination").and_then(Value::as_str)?.to_string(),
                        // `RW: false` and `Mode` containing "ro" both mean
                        // read-only, and different runtimes report different
                        // ones. Defaulting to read-write when neither is
                        // present matches the runtimes' own default.
                        read_only: m
                            .get("RW")
                            .and_then(Value::as_bool)
                            .map(|rw| !rw)
                            .unwrap_or(
                                m.get("Mode")
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| s.split(',').any(|o| o == "ro")),
                            ),
                        kind: m.get("Type").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ips(inspect: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let ns = inspect.get("NetworkSettings");
    if let Some(ip) = ns
        .and_then(|n| n.get("IPAddress"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        out.push(ip.to_string());
    }
    if let Some(nets) = ns
        .and_then(|n| n.get("Networks"))
        .and_then(Value::as_object)
    {
        for net in nets.values() {
            for key in ["IPAddress", "GlobalIPv6Address"] {
                if let Some(ip) = net
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(ip.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// ── Value helpers ────────────────────────────────────────────────────────────

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

fn first_str(candidates: &[Option<&Value>]) -> Option<String> {
    candidates
        .iter()
        .flatten()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        .map(str::to_string)
}

fn first_num(candidates: &[Option<&Value>]) -> Option<f64> {
    candidates.iter().flatten().find_map(|v| as_num(v))
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(as_num)
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Enough of RFC 3339 to read a container timestamp: `2026-08-28T02:00:01Z`
/// and the fractional/offset forms the runtimes emit.
///
/// Hand-rolled because a whole date library for one field would be the tail
/// wagging the dog, and because a wrong answer here is visible (`created_at`
/// in the document) rather than silent.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // A zero year is podman's "never started" sentinel (0001-01-01T00:00:00Z).
    if y <= 1 {
        return None;
    }
    // Days from the civil epoch — Howard Hinnant's algorithm, which is exact
    // for every proleptic Gregorian date and needs no table.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    // Trailing offset, if any: `+02:00` / `-05:30`.
    if let Some(pos) = s.rfind(['+', '-']).filter(|p| *p > 18)
        && let (Some(oh), Some(om)) = (
            s.get(pos + 1..pos + 3).and_then(|x| x.parse::<i64>().ok()),
            s.get(pos + 4..pos + 6).and_then(|x| x.parse::<i64>().ok()),
        )
    {
        let off = oh * 3600 + om * 60;
        secs += if s.as_bytes()[pos] == b'+' { -off } else { off };
    }
    Some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inspect_of(state: Value, config: Value) -> Value {
        json!({
            "Id": "abc123def456789",
            "Name": "caddy",
            "Created": "2026-08-01T10:00:00Z",
            "ImageDigest": "sha256:aaaa",
            "State": state,
            "Config": config,
        })
    }

    /// The garage case, exactly: a healthcheck that is configured, reports
    /// `unhealthy`, and has an EMPTY log — because the probe could never
    /// execute. Reporting that as a failing service is what happened for
    /// weeks; reporting it as healthy would be worse.
    #[test]
    fn a_healthcheck_that_never_ran_is_not_a_failing_one() {
        let c = build(
            &Value::Null,
            &inspect_of(
                json!({"Status": "running", "Health": {"Status": "unhealthy", "Log": []}}),
                json!({"Image": "docker.io/dxflrs/garage:v1.0", "Healthcheck": {"Test": ["CMD-SHELL", "true"]}}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(c.health, HealthState::NeverRan);
        assert_ne!(c.health, HealthState::Unhealthy);
    }

    #[test]
    fn a_healthcheck_with_results_reports_them() {
        let c = build(
            &Value::Null,
            &inspect_of(
                json!({"Status": "running",
                       "Health": {"Status": "unhealthy", "FailingStreak": 3,
                                  "Log": [{"ExitCode": 1}]}}),
                json!({"Image": "img", "Healthcheck": {"Test": ["CMD", "x"]}}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(c.health, HealthState::Unhealthy);
        assert_eq!(c.health_failing_streak, Some(3));
    }

    /// No healthcheck configured is not a fault and must not be graded.
    #[test]
    fn no_healthcheck_is_none_not_unhealthy() {
        let c = build(
            &Value::Null,
            &inspect_of(json!({"Status": "running"}), json!({"Image": "img"})),
            false,
        )
        .unwrap();
        assert_eq!(c.health, HealthState::None);
    }

    /// The join with the systemd sensor: without this label a container and
    /// its unit are two unrelated rows describing the same thing.
    #[test]
    fn the_quadlet_label_names_the_owning_unit() {
        let c = build(
            &Value::Null,
            &inspect_of(
                json!({"Status": "running"}),
                json!({"Image": "img", "Labels": {"PODMAN_SYSTEMD_UNIT": "caddy.service"}}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(c.unit.as_deref(), Some("caddy.service"));
    }

    /// A running container's `ExitCode` is 0, and publishing it would say
    /// "exited cleanly" about something that has not exited.
    #[test]
    fn a_running_container_reports_no_exit_code() {
        let running = build(
            &Value::Null,
            &inspect_of(
                json!({"Status": "running", "ExitCode": 0}),
                json!({"Image": "i"}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(running.exit_code, None);
        let exited = build(
            &Value::Null,
            &inspect_of(
                json!({"Status": "exited", "ExitCode": 137}),
                json!({"Image": "i"}),
            ),
            false,
        )
        .unwrap();
        assert_eq!(exited.exit_code, Some(137));
    }

    /// Docker prefixes names with a slash; leaving it on puts a `/` into every
    /// device slug and every key.
    #[test]
    fn a_docker_name_loses_its_leading_slash() {
        let mut i = inspect_of(json!({"Status": "running"}), json!({"Image": "i"}));
        i["Name"] = json!("/caddy");
        assert_eq!(build(&Value::Null, &i, false).unwrap().name, "caddy");
    }

    #[test]
    fn only_a_real_digest_is_reported_as_one() {
        let mut i = inspect_of(json!({"Status": "running"}), json!({"Image": "i"}));
        i["ImageDigest"] = json!("not-a-digest");
        i["Image"] = json!("docker.io/library/caddy:2.11.4");
        assert_eq!(build(&Value::Null, &i, false).unwrap().image.digest, None);
    }

    #[test]
    fn ports_and_mounts_are_read_from_the_inspect_document() {
        let mut i = inspect_of(json!({"Status": "running"}), json!({"Image": "i"}));
        i["NetworkSettings"] = json!({
            "Ports": {"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8080"}]},
            "IPAddress": "10.89.0.5"
        });
        i["Mounts"] = json!([
            {"Type": "bind", "Source": "/srv/data", "Destination": "/data", "RW": false},
            {"Type": "volume", "Destination": "/cache", "RW": true},
        ]);
        let c = build(&Value::Null, &i, false).unwrap();
        assert_eq!(c.ports.len(), 1);
        assert_eq!(c.ports[0].container_port, 80);
        assert_eq!(c.ports[0].host_port, Some(8080));
        assert_eq!(c.mounts.len(), 2);
        assert!(c.mounts[0].read_only);
        assert!(!c.mounts[1].read_only);
        assert_eq!(c.ips, vec!["10.89.0.5"]);
    }

    #[test]
    fn timestamps_parse_including_offsets_and_the_never_started_sentinel() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-28T02:00:01Z"), Some(1787882401));
        assert_eq!(
            parse_rfc3339("2026-08-28T04:00:01+02:00"),
            Some(1787882401),
            "an offset is applied, not ignored"
        );
        assert_eq!(
            parse_rfc3339("0001-01-01T00:00:00Z"),
            None,
            "podman's never-started sentinel is not a date"
        );
        assert_eq!(parse_rfc3339("nope"), None);
    }
}
