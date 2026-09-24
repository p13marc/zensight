//! The one-shot `--diagnose` mode (#947), the container twin of pve's (#880).
//!
//! Every rule in [`crate::alerts`] reads a field that can be *silent*: a
//! healthcheck the runtime never ran, a cgroup the sensor cannot read
//! (rootless, or a Docker socket that reports no path), an `oom_kill` counter
//! that is absent rather than zero, a signature that was never looked for.
//! The sensor is careful to report each silence as a silence — but an
//! operator still has to find out **which** silence they have, and reading a
//! sensor's tracing output at `debug` to find that out is not a diagnosis, it
//! is an investigation.
//!
//! So: one command, plain sentences, every socket the sensor would poll and
//! every field the seven rules depend on, with what came back and, when
//! nothing came back, why that is normal or is not. It never opens a Zenoh
//! session and never publishes anything — an operator debugging a socket
//! permission should not thereby join a fleet, and the way to guarantee that
//! is to never build the thing that would.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use zensight_common::container::{ContainerInfo, HealthState, SignatureState};

use crate::config::ContainerConfig;
use crate::runtime::{RuntimeClient, default_sockets};
use crate::upstream::UpstreamChecker;

/// `zensight-sensor-container` arguments: the framework's, plus the one-shot
/// mode.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "zensight-sensor-container",
    about = "OCI container sensor (#819)"
)]
pub struct ContainerArgs {
    #[command(flatten)]
    pub common: zensight_sensor_core::SensorArgs,

    /// Ask every configured runtime socket what it will actually answer — the
    /// containers, and for each the healthcheck, restart, exit, image and
    /// cgroup facts the rules depend on — print it, and exit. Read-only, and
    /// never touches the bus.
    #[arg(long)]
    pub diagnose: bool,
}

impl ContainerArgs {
    pub fn parse_with_default(default_config: &'static str) -> Self {
        let matches = <Self as clap::CommandFactory>::command()
            .mut_arg("config", |arg| arg.default_value(default_config))
            .get_matches();
        <Self as clap::FromArgMatches>::from_arg_matches(&matches)
            .expect("Failed to parse arguments")
    }
}

/// The sockets the sensor will poll, as `(path, rootless)`.
///
/// An explicit list wins; otherwise the conventional paths, and only the ones
/// that exist — listing a socket that is not there would make every cycle
/// log a failure for a runtime this host does not run. Shared by the sensor
/// and by `--diagnose`, so the diagnosis reports the same sockets the sensor
/// would poll; the diagnosis additionally names the conventional ones that
/// are *absent*, since "not found" is the first thing an operator asks.
pub fn socket_candidates(cfg: &ContainerConfig) -> Vec<(PathBuf, bool)> {
    if cfg.sockets.is_empty() {
        default_sockets()
            .into_iter()
            .filter(|(p, _)| p.exists())
            .collect()
    } else {
        cfg.sockets
            .iter()
            .map(|s| {
                // A socket under a user runtime dir is a rootless session, and
                // that changes where its containers' cgroups live.
                let rootless = s.contains("/run/user/");
                (PathBuf::from(s), rootless)
            })
            .collect()
    }
}

/// Run the diagnosis and print it. Returns once everything has been said.
pub async fn diagnose(cfg: &ContainerConfig) -> Result<()> {
    let timeout = Duration::from_secs(cfg.timeout_secs);
    println!("zensight-sensor-container --diagnose");
    println!(
        "source (the reporting host these series are filed under): {}",
        cfg.resolved_source()
    );
    println!("cgroup root: {}", cfg.cgroup_root);
    println!();

    println!("── Sockets ─────────────────────────────────────────────────────");
    if cfg.sockets.is_empty() {
        println!("  container.sockets is empty, so the conventional paths are tried in order:");
        for (p, rootless) in default_sockets() {
            println!(
                "    {} ({}): {}",
                p.display(),
                if rootless { "rootless" } else { "rootful" },
                if p.exists() {
                    "present — will be polled"
                } else {
                    "absent — skipped, which is normal for a runtime this host does not run"
                }
            );
        }
    } else {
        println!("  container.sockets is explicit; each is polled whether or not it exists:");
        for (p, rootless) in socket_candidates(cfg) {
            println!(
                "    {} ({}): {}",
                p.display(),
                if rootless { "rootless" } else { "rootful" },
                if p.exists() { "present" } else { "ABSENT" }
            );
        }
    }
    let clients: Vec<Arc<RuntimeClient>> = socket_candidates(cfg)
        .into_iter()
        .map(|(p, rootless)| Arc::new(RuntimeClient::new(p, timeout, rootless)))
        .collect();
    if clients.is_empty() {
        println!();
        println!("  No socket to ask. The sensor would keep running and report this in its");
        println!("  health document; set container.sockets if yours is elsewhere. A rootless");
        println!("  podman needs `systemctl --user start podman.socket` to have one at all.");
        return Ok(());
    }

    // The egress block is the one part of this sensor that leaves the host,
    // and it is built here only when the operator has turned it on — the
    // diagnosis must not reach a registry a running sensor would not.
    let upstream = if cfg.upstream.enabled {
        match UpstreamChecker::new(&cfg.upstream, timeout) {
            Ok(u) => Some(u),
            Err(e) => {
                println!();
                println!("  upstream: the checker could not be built: {e}");
                None
            }
        }
    } else {
        None
    };

    let cgroup_root = Path::new(&cfg.cgroup_root);
    for client in &clients {
        println!();
        println!(
            "── {} ({}) ─────────────────────────────────────",
            client.socket().display(),
            if client.rootless {
                "rootless"
            } else {
                "rootful"
            }
        );
        let list = match client.list().await {
            Ok(l) => l,
            Err(e) => {
                println!("  listing FAILED: {e}");
                println!(
                    "  Nothing on this socket can be asked. `Permission denied` is the socket's\n  \
                     mode or group (the sensor runs as whom?); `connection refused` is a socket\n  \
                     file with no podman behind it; a 404 is a socket that speaks only the\n  \
                     Docker compatibility API on a path this sensor asks in libpod's dialect."
                );
                continue;
            }
        };
        println!("  {} container(s) listed (running or not)", list.len());
        if list.is_empty() {
            println!("  Zero here is a real answer: the runtime is reachable and has nothing.");
        }

        for entry in &list {
            let Some(id) = entry.get("Id").and_then(|v| v.as_str()) else {
                println!("  a list entry has no `Id` — skipped, as the sensor would skip it");
                continue;
            };
            let inspect = match client.inspect(id).await {
                Ok(serde_json::Value::Array(mut a)) if !a.is_empty() => a.remove(0),
                Ok(v) => v,
                Err(e) => {
                    println!("  {}: inspect FAILED: {e}", short(id));
                    continue;
                }
            };
            let Some(mut info) = crate::inspect::build(entry, &inspect, client.rootless) else {
                println!(
                    "  {}: the inspect document has no name or no image reference — the \
                     sensor would skip it",
                    short(id)
                );
                continue;
            };
            if cfg.ignore.contains(&info.name) {
                println!("  {} — in container.ignore, skipped", info.name);
                continue;
            }
            let cgroup_dir =
                crate::cgroup::resolve(cgroup_root, info.cgroup_path.as_deref(), &info.id);
            if let Some(dir) = &cgroup_dir {
                info.resources = crate::cgroup::read_resources(dir);
            }
            let mut reasons = Reasons::default();
            if let Some(checker) = &upstream {
                match checker.digest_lookup(&info.image.reference).await {
                    Ok(d) => info.image.upstream_digest = Some(d),
                    Err(why) => reasons.digest = Some(why),
                }
                info.image.signature = match info.image.digest.as_deref() {
                    Some(d) => match checker.signature_lookup(&info.image.reference, d).await {
                        Ok(true) => SignatureState::Present,
                        Ok(false) => SignatureState::Absent,
                        Err(why) => {
                            reasons.signature = Some(why);
                            SignatureState::NotChecked
                        }
                    },
                    None => {
                        reasons.signature = Some("the running digest is unknown".into());
                        SignatureState::NotChecked
                    }
                };
            }
            print_container(
                cfg,
                &info,
                cgroup_dir.as_deref(),
                upstream.is_some(),
                &reasons,
            );
        }
    }
    Ok(())
}

/// Why an upstream question went unanswered — the diagnosis's whole reason
/// to exist is to say this out loud where the sensor says nothing.
#[derive(Default)]
struct Reasons {
    digest: Option<String>,
    signature: Option<String>,
}

/// One container: every field a rule reads, in the order of the rule table.
fn print_container(
    cfg: &ContainerConfig,
    c: &ContainerInfo,
    cgroup_dir: Option<&Path>,
    egress: bool,
    why: &Reasons,
) {
    println!();
    println!(
        "  {} — {}{}",
        c.name,
        c.status,
        match c.exit_code {
            Some(code) if !c.is_running() => format!(" (exit code {code})"),
            _ => String::new(),
        }
    );
    if cfg.alerts.exempt.contains(&c.name) {
        println!("      in container.alerts.exempt: observed, never graded");
    }
    println!(
        "      unit: {}",
        c.unit.as_deref().unwrap_or(
            "NONE — no PODMAN_SYSTEMD_UNIT label, so this container will not join up with \
             the systemd sensor's view"
        )
    );
    println!(
        "      restart policy {}, restart count {} (a rule about the DELTA within {}s, so a \
         count alone never fires)",
        c.restart_policy.as_deref().unwrap_or("unknown"),
        c.restart_count,
        cfg.alerts.restart_window_secs
    );

    // Health — the garage case.
    let verdict = match c.health {
        HealthState::None => "no healthcheck configured — nothing to say; not a fault".to_string(),
        HealthState::NeverRan => "NEVER RAN — configured, and it has never produced a result: \
             the probe cannot run (a CMD-SHELL check in an image with no shell is the usual \
             cause). The service may be perfectly healthy; `health-never-ran` fires, \
             `unhealthy` does not"
            .to_string(),
        HealthState::Unhealthy => format!(
            "UNHEALTHY{} — `unhealthy` fires",
            match c.health_failing_streak {
                Some(n) if n > 0 => format!(" ({n} consecutive failing probes)"),
                _ => String::new(),
            }
        ),
        HealthState::Healthy => "healthy".to_string(),
        HealthState::Starting => "starting — in its start period, no verdict yet".to_string(),
    };
    println!("      health: {verdict}");

    // Exit.
    match (c.is_running(), c.exit_code) {
        (false, Some(code)) if code != 0 => {
            println!("      exit: code {code} and not running — `exited-nonzero` fires")
        }
        (false, Some(_)) => println!("      exit: clean (0) — a clean exit is not a fault"),
        (false, None) => println!(
            "      exit: not running and the runtime reports no exit code — `exited-nonzero` \
             cannot fire on a code it was not given"
        ),
        (true, _) => {}
    }

    // Image.
    println!("      image: {}", c.image.reference);
    println!(
        "      digest running: {}",
        c.image.digest.as_deref().unwrap_or(
            "NOT REPORTED — the Docker compatibility API omits it; `image-behind` and the \
             signature check need libpod's dialect"
        )
    );
    if egress {
        println!(
            "      digest upstream: {}",
            match (&c.image.upstream_digest, &c.image.digest) {
                (Some(u), Some(d)) if u == d => format!("{u} — same as running, up to date"),
                (Some(u), Some(_)) => format!("{u} — DIFFERS from running; `image-behind` fires"),
                (Some(u), None) =>
                    format!("{u} — but the running digest is unknown, so no comparison"),
                (None, _) => format!(
                    "NOT RESOLVED — {}; `image-behind` cannot fire",
                    why.digest.as_deref().unwrap_or("no reason recorded")
                ),
            }
        );
        println!(
            "      signature: {}",
            match c.image.signature {
                SignatureState::Present => "present".to_string(),
                SignatureState::Absent => "ABSENT — `unsigned` fires".to_string(),
                SignatureState::NotChecked => format!(
                    "not checked — {}: silence, never \"unsigned\"",
                    why.signature.as_deref().unwrap_or("no reason recorded")
                ),
            }
        );
    } else {
        println!(
            "      digest upstream / signature: not asked — container.upstream.enabled is \
             false, so `image-behind` and `unsigned` can never fire (and nothing leaves \
             this host)"
        );
    }

    // cgroup.
    match cgroup_dir {
        Some(dir) => {
            println!(
                "      cgroup: {}{}",
                dir.display(),
                if c.cgroup_path.is_none() {
                    " (guessed — the runtime reported no path)"
                } else {
                    ""
                }
            );
            let r = &c.resources;
            println!(
                "          memory.current {}, memory.max {}, memory.peak {}",
                opt_bytes(r.memory_bytes, "unreadable"),
                match r.memory_max_bytes {
                    Some(m) => human_bytes(m),
                    None => "none (`max`, no limit — not a limit of zero)".to_string(),
                },
                opt_bytes(
                    r.memory_peak_bytes,
                    "unreadable (older kernels have no memory.peak)"
                )
            );
            println!(
                "          oom_kill {}, memory.events max {}",
                match r.oom_kills {
                    Some(n) => format!(
                        "{n} (cumulative; `oom-killed` fires on a rise, never on the total)"
                    ),
                    None =>
                        "UNREADABLE — memory.events is missing, so `oom-killed` can never fire \
                         for this container"
                            .to_string(),
                },
                r.memory_max_events
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unreadable".into())
            );
            println!(
                "          cpu usage {}, throttled {}, pids {}",
                r.cpu_usage_usec
                    .map(|u| format!("{}s", u / 1_000_000))
                    .unwrap_or_else(|| "unreadable".into()),
                r.cpu_throttled_usec
                    .map(|u| format!("{}s", u / 1_000_000))
                    .unwrap_or_else(|| "unreadable".into()),
                r.pids
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unreadable".into())
            );
            println!(
                "          pressure avg10 cpu {}, memory {}, io {}",
                opt_pct(r.cpu_pressure_avg10),
                opt_pct(r.memory_pressure_avg10),
                opt_pct(r.io_pressure_avg10)
            );
        }
        None => {
            println!(
                "      cgroup: NOT FOUND — reported path {}, and the rootful guess \
                 machine.slice/libpod-<id>.scope is not a directory under {}. Every resource \
                 series is absent and `oom-killed` can never fire.{}",
                c.cgroup_path.as_deref().unwrap_or("(none)"),
                cfg.cgroup_root,
                if c.rootless {
                    " This is a rootless container: its cgroup is under user.slice and is \
                     unreadable from another user's session, and unreadable from inside a \
                     container unless the cgroup tree is mounted in."
                } else {
                    ""
                }
            );
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn opt_bytes(b: Option<u64>, absent: &str) -> String {
    b.map(human_bytes).unwrap_or_else(|| absent.to_string())
}

fn opt_pct(p: Option<f64>) -> String {
    p.map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "off".to_string())
}

fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let b = b as f64;
    if b >= K * K * K {
        format!("{:.1} GiB", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.1} MiB", b / (K * K))
    } else if b >= K {
        format!("{:.0} KiB", b / K)
    } else {
        format!("{b} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_sockets_are_polled_whether_or_not_they_exist() {
        let cfg = ContainerConfig {
            sockets: vec![
                "/nonexistent/podman.sock".into(),
                "/run/user/1000/podman/podman.sock".into(),
            ],
            ..Default::default()
        };
        let s = socket_candidates(&cfg);
        assert_eq!(s.len(), 2, "an explicit socket is never filtered out");
        assert!(!s[0].1, "a system path is rootful");
        assert!(s[1].1, "a /run/user path is rootless");
    }

    #[test]
    fn discovered_sockets_are_only_the_present_ones() {
        let cfg = ContainerConfig::default();
        for (p, _) in socket_candidates(&cfg) {
            assert!(p.exists(), "{} was listed but is absent", p.display());
        }
    }

    #[test]
    fn human_bytes_keeps_the_unit_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        assert_eq!(human_bytes(268_435_456), "256.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn a_short_id_is_not_sliced_past_its_end() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
    }
}
