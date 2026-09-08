//! `zensight-conformance` — run zenkey-fleet's RFC judges against a live
//! ZenSight deployment and exit on what they find (#744).
//!
//! `cargo test --workspace` proves ZenSight's code agrees with itself. It says
//! nothing about whether what a running sensor *puts on the wire* agrees with
//! the keyspace-v2 RFCs and with the registry TOMLs it claims to serve — the
//! served-vs-declared slice diff, `alive ⇒ callable`, schema drift, declared
//! QoS vs observed, cardinality budgets. Those are properties of a deployment,
//! and only a deployment can be asked.
//!
//! This binary asks. It opens an un-namespaced observer session, runs
//! [`zenkey_fleet::run_doctor`] — the same entry point `zenctl doctor` and the
//! zengui doctor panel call, so a finding here is a finding there — and folds
//! the report into one RFC 13 judgement through [`gate`].
//!
//! `scripts/conformance-verify.sh` is what stands a deployment up in front of
//! it; the CI job is `conformance` in `.forgejo/workflows/ci.yml`. Pointed at
//! `--connect` for a real bus it judges that instead, unchanged.
//!
//! # Namespaces
//!
//! The session is deliberately **un-namespaced** (RFC 09 §5): an explorer that
//! strips a prefix on ingress cannot see a key that leaked outside it, so
//! `zenkey_fleet::open_with_config` *rejects* a zenoh config file that sets
//! one. ZenSight's own `zenoh.namespace` is empty by default, so its keys sit
//! at the bus root and this costs nothing today. A deployment that does set a
//! base names it with `--base` here — the base is a `Fleet` field, never a
//! session namespace.

mod gate;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use zenkey_fleet::report::{Asked, CheckId, DoctorFinding, DoctorReport, DoctorSeverity};
use zenkey_fleet::{DoctorSpec, Fleet, Judgement, SliceSet, judgement_exit_code, run_doctor};

use gate::{DEFAULT_EXCLUDED, FailOn, Gate, Verdict};

/// Where the registry TOMLs live, relative to this crate. Only a default: the
/// harness and CI pass `--registry` explicitly, and a binary copied out of the
/// target dir gets a clear error rather than a silently registry-less run
/// (which would make the served-vs-declared diff `NotAsked` — RFC 09 §5.1 O4 —
/// and quietly delete the most valuable check in the set).
const DEFAULT_REGISTRY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../zensight-common/registry");

#[derive(Parser, Debug)]
#[command(
    name = "zensight-conformance",
    about = "Judge a live ZenSight deployment against the keyspace-v2 RFCs",
    long_about = None,
)]
struct Args {
    /// Zenoh endpoint(s) to dial (repeatable). The deployment's rendezvous.
    #[arg(long, value_name = "ENDPOINT")]
    connect: Vec<String>,

    /// Zenoh endpoint(s) to listen on (repeatable).
    #[arg(long, value_name = "ENDPOINT")]
    listen: Vec<String>,

    /// Turn multicast scouting on. Off by default: a conformance run must
    /// judge the deployment it was pointed at, not whatever else answers on
    /// the LAN (RFC 09 §0.1's contamination warning).
    #[arg(long)]
    scouting: bool,

    /// The deployment base (`zenoh.namespace`). Empty is the default and is a
    /// real deployment — the bus-root one — never "no base".
    #[arg(long, default_value = "")]
    base: String,

    /// Directory of registry TOMLs to diff the served slices against
    /// (repeatable).
    #[arg(long, value_name = "DIR")]
    registry: Vec<PathBuf>,

    /// Skip the deep checks (per-family state snapshots for freshness, the
    /// storage-coverage join). They are on by default: they are most of what
    /// distinguishes this from a liveness probe.
    #[arg(long)]
    shallow: bool,

    /// At most this many state samples drained per family in the deep checks.
    #[arg(long, default_value_t = 64, value_name = "N")]
    sample: usize,

    /// Per-query reply timeout, in seconds.
    #[arg(long, default_value_t = 3.0, value_name = "SECS")]
    timeout: f64,

    /// Listen passively to the data planes for this long after the GET fan-in
    /// and judge what rides. `0` skips the phase entirely, and the report then
    /// carries no observation section at all rather than an empty one.
    #[arg(long = "for", default_value_t = 10.0, value_name = "SECS")]
    for_secs: f64,

    /// Severity floor a finding must reach to fail the run.
    #[arg(long, value_enum, default_value = "warning")]
    fail_on: FailOn,

    /// Exclude a check id from the gate (repeatable). Adds to the built-in
    /// exclusions; `--list-checks` prints every id.
    #[arg(long = "allow", value_name = "CHECK-ID")]
    allow: Vec<String>,

    /// Put a built-in exclusion back under the gate (repeatable). The
    /// built-in list is empty since #845; this stays as the lever for
    /// whenever an exclusion returns.
    #[arg(long = "deny", value_name = "CHECK-ID")]
    deny: Vec<String>,

    /// Treat a listen window that dropped samples as unobservable rather than
    /// clean (RFC 09 §5.1 O6). Off by default — see `gate::Gate::strict_window`.
    #[arg(long)]
    strict_window: bool,

    /// Record a `.zrec` capture to this path instead of judging (#747): watch
    /// `--record-selector` for `--for` seconds (and/or `--record-max` samples)
    /// and write what rides. The capture is the GUI's fixture corpus source —
    /// this crate is the sanctioned place to drive zenkey-fleet against a live
    /// deployment, so the recorder lives beside the judges rather than growing
    /// a third harness.
    #[arg(long, value_name = "PATH")]
    record_zrec: Option<PathBuf>,

    /// Data-plane selector to record (repeatable; required with
    /// `--record-zrec`). Full wire selectors — remember `*` never matches a
    /// verbatim origin (grammar D4), so `@catalog` traffic needs its own
    /// entry.
    #[arg(long, value_name = "SELECTOR")]
    record_selector: Vec<String>,

    /// Stop the capture after this many samples. `0` means no sample cap —
    /// the `--for` deadline is then the only bound.
    #[arg(long, default_value_t = 0, value_name = "N")]
    record_max: u64,

    /// Emit the whole doctor report as JSON (plus the gate's verdict) instead
    /// of the human summary.
    #[arg(long)]
    json: bool,

    /// Print every check id the doctor can emit, and which are excluded by
    /// default, then exit.
    #[arg(long)]
    list_checks: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_checks {
        for id in CheckId::ALL {
            let mark = if DEFAULT_EXCLUDED.contains(&id) {
                "excluded by default"
            } else {
                ""
            };
            println!("{:<28} {mark}", id.as_str());
        }
        return Ok(());
    }

    if let Some(path) = &args.record_zrec {
        return record_capture(&args, path).await;
    }

    let gate = build_gate(&args)?;
    let dirs = registry_dirs(&args)?;
    let slices = SliceSet::from_dirs(&dirs)
        .with_context(|| format!("failed to load the registry from {dirs:?}"))?;
    if slices.slices().is_empty() {
        bail!(
            "the registry directories {dirs:?} declare no slices — the \
             served-vs-declared diff would never run, and a report that never \
             ran the diff must not read as a clean one (RFC 09 §5.1 O4)"
        );
    }

    let spec = DoctorSpec {
        deep: !args.shallow,
        sample: Some(args.sample),
        timeout: secs(args.timeout, "--timeout")?,
        listen: (args.for_secs > 0.0)
            .then(|| secs(args.for_secs, "--for"))
            .transpose()?,
    };

    // `open`, not `open_with_config`: there is no zenoh file to pass through,
    // and the three knobs are the whole surface a conformance run needs.
    let session = zenkey_fleet::open(&args.connect, &args.listen, args.scouting)
        .await
        .context("could not open the observer session")?;
    let fleet = Fleet::new(&session, &args.base);

    let report = run_doctor(&fleet, Some(&slices), &spec)
        .await
        .context("the doctor run itself failed")?;
    let verdict = gate::judge(&report, &gate);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "report": report,
                "verdict": {
                    "judgement": verdict.judgement,
                    "gated": verdict.gated,
                    "excluded": verdict.excluded,
                    "below_floor": verdict.below_floor,
                },
            }))?
        );
    } else {
        print_summary(&report, &verdict, &gate);
    }

    // The session is closed rather than dropped: an observer that vanishes
    // mid-teardown leaves its subscriptions to time out on every peer it was
    // judging, which is a poor last act for a tool whose whole subject is
    // whether the bus is well behaved.
    let _ = session.close().await;

    // The exit code IS the report as far as CI is concerned, so it is the last
    // thing that happens and it goes through the upstream projection rather
    // than a hand-rolled match.
    std::process::exit(judgement_exit_code(&verdict.judgement));
}

/// Record mode (#747): a passive capture of whatever rides the given
/// selectors, written in the `.zrec` dialect (RFC 09 §5.2). No judging, no
/// registry — a capture is an observation, and the header's `selectors` field
/// is its coverage statement. The session is opened exactly as a doctor run's
/// (same endpoints, scouting off by default), so the isolation discipline in
/// `scripts/conformance-verify.sh` carries over unchanged.
async fn record_capture(args: &Args, path: &std::path::Path) -> Result<()> {
    use zenkey_fleet::report::ZrecHeader;
    use zenkey_fleet::{Monitor, MonitorSpec, RecordBounds, ZREC_VERSION, ZrecSink, record};

    if args.record_selector.is_empty() {
        bail!(
            "--record-zrec needs at least one --record-selector — a capture with no selectors watches nothing and its header could state no coverage"
        );
    }
    let window = secs(args.for_secs, "--for")?;

    let session = zenkey_fleet::open(&args.connect, &args.listen, args.scouting)
        .await
        .context("could not open the observer session")?;
    let monitor = Monitor::start(
        &session,
        MonitorSpec {
            selectors: args.record_selector.clone(),
            ..MonitorSpec::default()
        },
    )
    .await
    .context("could not start the monitor")?;

    let header = ZrecHeader {
        zrec: ZREC_VERSION,
        selectors: args.record_selector.clone(),
        base: args.base.clone(),
        captured_at: zenkey_fleet::rfc3339_now(),
        // Both absent, and that is the coverage statement, not an omission:
        // this is a live untriggered capture, so it fetched no state ahead of
        // the window (`preamble`) and read no retained ring behind it
        // (`pre_roll`). A header that claimed either would claim coverage the
        // file does not have.
        preamble: None,
        pre_roll: None,
    };
    let out = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let sink = ZrecSink::spawn(out, &header)
        .await
        .context("could not start the capture sink")?;

    let mut events = monitor.events();
    record(
        &mut events,
        &sink,
        RecordBounds {
            max_samples: (args.record_max > 0).then_some(args.record_max),
            max_duration: Some(window),
        },
        |_, _| {},
    )
    .await
    .context("the capture itself failed")?;

    let counts = sink.finish().await.context("could not flush the capture")?;
    let (samples, dropped) = (counts.samples, counts.dropped);
    monitor.shutdown().await.ok();
    let _ = session.close().await;

    println!(
        "recorded {} sample(s) ({} dropped by the capture) to {}",
        samples,
        dropped,
        path.display()
    );
    if samples == 0 {
        // An empty capture is not a fixture; failing loudly here is what keeps
        // a broken regeneration run from silently emptying the corpus.
        bail!("the capture holds no samples — nothing rode the selectors within the window");
    }
    Ok(())
}

fn secs(value: f64, flag: &str) -> Result<Duration> {
    if !(value.is_finite() && value > 0.0) {
        bail!("{flag} must be a positive number of seconds, got {value}");
    }
    Ok(Duration::from_secs_f64(value))
}

fn build_gate(args: &Args) -> Result<Gate> {
    let parse = |tokens: &[String]| -> Result<Vec<CheckId>> {
        tokens
            .iter()
            .map(|t| {
                CheckId::parse(t).with_context(|| {
                    format!("unknown check id {t:?} — run with --list-checks for the set")
                })
            })
            .collect()
    };
    let denied = parse(&args.deny)?;
    let mut excluded: Vec<CheckId> = DEFAULT_EXCLUDED
        .iter()
        .copied()
        .filter(|id| !denied.contains(id))
        .collect();
    for id in parse(&args.allow)? {
        if !excluded.contains(&id) {
            excluded.push(id);
        }
    }
    Ok(Gate {
        fail_on: args.fail_on,
        excluded,
        strict_window: args.strict_window,
    })
}

fn registry_dirs(args: &Args) -> Result<Vec<PathBuf>> {
    if !args.registry.is_empty() {
        return Ok(args.registry.clone());
    }
    let fallback = PathBuf::from(DEFAULT_REGISTRY);
    if fallback.is_dir() {
        return Ok(vec![fallback]);
    }
    bail!(
        "no --registry given and the in-tree default ({}) is not a directory. \
         The served-vs-declared slice diff is the point of this tool; it does \
         not run without one.",
        fallback.display()
    )
}

fn print_summary(report: &DoctorReport, verdict: &Verdict, gate: &Gate) {
    println!("== zensight-conformance ==");
    println!(
        "producers: {} live, {} answered introspect, {} serve describe ({} do not)",
        report.live_producers,
        report.introspect_answered,
        report.describe_served,
        report.describe_missing,
    );
    match &report.synced {
        // O4 again: "the diff never ran" and "the diff confirmed nothing" are
        // different sentences and this is the only place either gets said.
        Asked::NotAsked => println!("slices in sync: THE DIFF NEVER RAN (no registry)"),
        Asked::Asked(rows) if rows.is_empty() => {
            println!("slices in sync: none — the diff ran and confirmed nothing")
        }
        Asked::Asked(rows) => {
            println!("slices in sync: {}", rows.len());
            for row in rows {
                println!("    {row}");
            }
        }
    }
    println!(
        "routers: {}{}   deep checks: {}",
        report.routers,
        report
            .router_version
            .as_deref()
            .map(|v| format!(" (zenoh {v})"))
            .unwrap_or_default(),
        if report.deep { "ran" } else { "skipped" },
    );
    if let Some(o) = &report.observation {
        println!(
            "listen window: {:.1}s, {} sample(s) over {} key(s); scopes: {}",
            o.window_s,
            o.samples,
            o.keys_seen,
            o.scopes.join(", ")
        );
        // Printed unconditionally, at zero too: "we watched and dropped
        // nothing" is a claim worth making, and a number that only appears
        // when it is bad trains a reader to skip it.
        println!(
            "    dropped: {} sample(s), {} field path(s) refused, {} key projection(s) evicted, \
             {} synthetic sample(s)",
            o.dropped, o.field_paths_dropped, o.facts_evicted, o.synthetic_marked,
        );
    } else {
        println!("listen window: not run (--for 0)");
    }

    section("GATED (these fail the run)", &verdict.gated);
    section("below the severity floor", &verdict.below_floor);
    if !verdict.excluded.is_empty() {
        let mut ids: Vec<&str> = gate.excluded.iter().map(|c| c.as_str()).collect();
        ids.sort_unstable();
        println!(
            "\n-- EXCLUDED FROM THE GATE ({}) — {} --",
            verdict.excluded.len(),
            ids.join(", ")
        );
        println!("   Not clean, just not gated. Every id here came from --allow (the");
        println!("   built-in exclusion list is empty since #845); drop the flag to");
        println!("   put a check back under the gate.");
        summarize(&verdict.excluded);
    }

    println!();
    match &verdict.judgement {
        Judgement::NotEstablished { reason } => println!("PASS — no gated findings. {reason}"),
        Judgement::Established => println!(
            "FAIL — {} gated finding(s); the deployment does not conform.",
            verdict.gated.len()
        ),
        Judgement::Unobservable { reason } => {
            println!("UNOBSERVABLE — the run cannot carry a verdict. {reason}")
        }
        Judgement::NotAsked => println!("NOT ASKED — the checks never ran."),
    }
}

fn section(title: &str, findings: &[DoctorFinding]) {
    if findings.is_empty() {
        return;
    }
    println!("\n-- {title} ({}) --", findings.len());
    for f in findings {
        println!(
            "  [{}] {} · {}: {}{}",
            severity(f.severity),
            f.check,
            f.subject,
            f.evidence,
            f.citation
                .as_deref()
                .map(|c| format!("  ({c})"))
                .unwrap_or_default(),
        );
    }
}

/// Excluded findings are counted per check id, not listed: the one that
/// motivated the exclusion produced 141 lines at four producers, and a wall of
/// known-false text is how a reader learns to scroll past the section that
/// also holds the real ones.
fn summarize(findings: &[DoctorFinding]) {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for f in findings {
        *counts.entry(f.check.as_str()).or_default() += 1;
    }
    for (id, n) in counts {
        println!("   {id}: {n} finding(s) suppressed");
    }
}

fn severity(s: DoctorSeverity) -> &'static str {
    match s {
        DoctorSeverity::Error => "error",
        DoctorSeverity::Warning => "warn",
        DoctorSeverity::Info => "info",
    }
}
