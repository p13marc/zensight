//! The one-shot `--diagnose` mode (#880).
//!
//! `volumes: 0` and a missing `allocated` are both *silences*, and the whole
//! point of this release's pve work is that a silence must never be reported
//! as a measurement. But an operator still has to find out **which** silence
//! they have — a pool that is not backup-capable, a token whose role is
//! narrower than `PVEAuditor`, a volid shape this sensor cannot read — and
//! reading a sensor's tracing output at `debug` to find that out is not a
//! diagnosis, it is an investigation.
//!
//! So: one command, plain sentences, every endpoint the backup and storage
//! paths depend on, with what came back. It never opens a Zenoh session and
//! never publishes anything — an operator debugging a token should not
//! thereby join a fleet, and the way to guarantee that is to never build the
//! thing that would.

use anyhow::Result;
use clap::Parser;

use crate::api::PveClient;
use crate::config::PveConfig;
use crate::poller::vmid_from_volid;

/// `zensight-sensor-pve` arguments: the framework's, plus the one-shot modes.
#[derive(Parser, Debug, Clone)]
#[command(name = "zensight-sensor-pve", about = "Proxmox VE sensor (#818)")]
pub struct PveArgs {
    #[command(flatten)]
    pub common: zensight_sensor_core::SensorArgs,

    /// Ask the configured API what it will actually answer — pools, backup
    /// content, vzdump tasks, guest disks — print it, and exit. Read-only,
    /// and never touches the bus.
    #[arg(long)]
    pub diagnose: bool,
}

impl PveArgs {
    pub fn parse_with_default(default_config: &'static str) -> Self {
        let matches = <Self as clap::CommandFactory>::command()
            .mut_arg("config", |arg| arg.default_value(default_config))
            .get_matches();
        <Self as clap::FromArgMatches>::from_arg_matches(&matches)
            .expect("Failed to parse arguments")
    }
}

/// Run the diagnosis and print it. Returns once everything has been said.
pub async fn diagnose(client: &PveClient, cfg: &PveConfig) -> Result<()> {
    println!("zensight-sensor-pve --diagnose against {}", cfg.base_url());
    println!(
        "source (the reporting host these series are filed under): {}",
        cfg.resolved_source()
    );
    println!();

    let (guests, pools) = match client.resources().await {
        Ok(v) => v,
        Err(e) => {
            println!("/cluster/resources FAILED: {e}");
            println!("\nNothing else can be asked without it. A 401 here is the token; a");
            println!("transport error is the endpoint, the port or the certificate.");
            return Ok(());
        }
    };
    println!(
        "/cluster/resources: {} guest(s), {} storage row(s)",
        guests.len(),
        pools.len()
    );

    println!("\n── Pools ───────────────────────────────────────────────────────");
    for p in &pools {
        let backup_capable = p.content.is_empty() || p.content.iter().any(|c| c == "backup");
        println!(
            "  {} on {} (type {}, enabled {}, content {:?}) — {} backup listing",
            p.storage,
            p.node,
            p.kind.as_deref().unwrap_or("unknown"),
            p.enabled,
            p.content,
            if p.enabled && backup_capable {
                "WILL ask for a"
            } else {
                "will NOT ask for a"
            },
        );
        match client.storage_allocated(&p.node, &p.storage).await {
            Ok(Some(bytes)) => println!("      allocated (reported by the plugin): {bytes} bytes"),
            Ok(None) => println!(
                "      allocated: NOT REPORTED — normal for a `dir` storage; the sensor \
                 derives it from the guest disks instead (#881), summed below"
            ),
            Err(e) => println!("      allocated: listing FAILED: {e}"),
        }
    }

    println!("\n── Backup content ──────────────────────────────────────────────");
    let mut any = false;
    for p in pools
        .iter()
        .filter(|p| p.enabled && (p.content.is_empty() || p.content.iter().any(|c| c == "backup")))
    {
        any = true;
        match client.backups(&p.node, &p.storage).await {
            Ok(vols) => {
                println!("  {}: {} volume(s)", p.storage, vols.len());
                let unreadable: Vec<&str> = vols
                    .iter()
                    .filter(|v| vmid_from_volid(&v.volid).is_none())
                    .map(|v| v.volid.as_str())
                    .collect();
                if !unreadable.is_empty() {
                    println!(
                        "      {} volid(s) name no guest this sensor can read — THIS is why a \
                         guest would show no volumes:",
                        unreadable.len()
                    );
                    for v in unreadable.iter().take(5) {
                        println!("        {v}");
                    }
                }
            }
            Err(e) => println!("  {}: listing FAILED: {e}", p.storage),
        }
    }
    if !any {
        println!("  No pool is both enabled and backup-capable, so no listing is attempted.");
        println!("  Every guest's `volumes` will be reported as unknown, which is correct.");
    }

    println!("\n── vzdump tasks ────────────────────────────────────────────────");
    let mut nodes: Vec<&str> = pools.iter().map(|p| p.node.as_str()).collect();
    nodes.sort_unstable();
    nodes.dedup();
    let now = zensight_common::current_timestamp_millis() / 1000;
    let max_age = cfg.alerts.backup_task_max_age_secs as i64;
    for node in nodes {
        match client.vzdump_tasks(node, 200).await {
            Ok(rows) => {
                println!(
                    "  {node}: {} per-guest task(s), {} WHOLE-JOB run(s) (`all 1`, no guest id)",
                    rows.per_guest.len(),
                    rows.jobs.len()
                );
                for (vmid, t) in rows.per_guest.iter().take(10) {
                    let age = now - t.started_at;
                    println!(
                        "      guest {vmid}: {} {} ago{}",
                        if t.ok { "OK" } else { "FAILED" },
                        human_secs(age.max(0) as u64),
                        if max_age > 0 && age > max_age {
                            " — TOO OLD to be evidence about the last backup"
                        } else {
                            ""
                        }
                    );
                }
                for t in rows.jobs.iter().take(5) {
                    println!(
                        "      whole job: {} {} ago",
                        if t.ok { "OK" } else { "FAILED" },
                        human_secs((now - t.started_at).max(0) as u64)
                    );
                }
            }
            Err(e) => println!("  {node}: task listing FAILED: {e}"),
        }
    }

    println!("\n── Guest disks (the derived `allocated`) ───────────────────────");
    let mut by_storage: std::collections::BTreeMap<String, (u64, usize)> = Default::default();
    for rt in &guests {
        match client.guest_config(rt).await {
            Ok(g) => {
                for d in &g.disks {
                    let Some(storage) = &d.storage else { continue };
                    let e = by_storage.entry(storage.clone()).or_default();
                    e.0 += d.size_bytes.unwrap_or(0);
                    e.1 += 1;
                }
            }
            Err(e) => println!("  guest {} config FAILED: {e}", rt.vmid),
        }
    }
    if by_storage.is_empty() {
        println!("  No guest disk names a storage — nothing can be derived.");
    }
    for (storage, (bytes, disks)) in by_storage {
        println!(
            "  {storage}: {disks} disk(s), {bytes} bytes provisioned (a FLOOR — detached \
             `unused<N>` volumes are not counted)"
        );
    }
    Ok(())
}

fn human_secs(s: u64) -> String {
    match s {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}
