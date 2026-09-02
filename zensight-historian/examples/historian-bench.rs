//! Measure the store's storage properties against #911's acceptance numbers.
//!
//! ```text
//! cargo run --release -p zensight-historian --example historian-bench -- \
//!     --series 10000 --minutes 120
//! ```
//!
//! # What this can and cannot answer
//!
//! #911 asks for six numbers measured on the reference fleet after fourteen
//! days. Five of them are properties of the **store** and can be measured in
//! minutes by writing the same rows the ingest path would: series count, bytes
//! per bucket on disk, database size at the defaults, prune wall time, and
//! range-query latency.
//!
//! The sixth — steady RSS under the governor's ladder — is a property of the
//! **running service under a real fleet's arrival pattern**, and no bench
//! reproduces it: the ring's occupancy depends on how many series are active
//! at once and how fast they arrive, not on how many exist. That one waits for
//! the soak, and this program says so rather than printing a number that would
//! be quoted as if it had been measured.
//!
//! It writes through `MetricStore::record` and `take_flush_batch` — the real
//! ingest seam — rather than constructing rows directly, so what it measures
//! is what the historian would write, downsampling and interning included.

use std::time::Instant;

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};
use zensight_store::{MetricStore, PersistentStore, Tier};

struct Args {
    series: usize,
    minutes: i64,
    path: std::path::PathBuf,
    keep: bool,
    /// Stop after ingest, having closed the store cleanly — so the file on
    /// disk can be examined by another process.
    ingest_only: bool,
}

fn parse_args() -> Args {
    let mut series = 10_000usize;
    let mut minutes = 120i64;
    let mut path = std::env::temp_dir().join(format!(
        "zensight-historian-bench-{}.redb",
        std::process::id()
    ));
    let mut keep = false;
    let mut ingest_only = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--series" => series = it.next().and_then(|v| v.parse().ok()).unwrap_or(series),
            "--minutes" => minutes = it.next().and_then(|v| v.parse().ok()).unwrap_or(minutes),
            "--path" => path = it.next().map(Into::into).unwrap_or(path),
            "--keep" => keep = true,
            "--ingest-only" => ingest_only = true,
            other => eprintln!("ignoring unknown argument {other:?}"),
        }
    }
    Args {
        series,
        minutes,
        path,
        keep,
        ingest_only,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    let _ = std::fs::remove_file(&args.path);

    println!("# zensight-historian storage bench (#911)");
    println!("series:  {}", args.series);
    println!("minutes: {}", args.minutes);
    println!("path:    {}", args.path.display());
    println!();

    let persistent = PersistentStore::open(&args.path)?;
    // The historian's tier set, not the GUI's: what this measures has to be
    // what the service writes.
    let mut store =
        MetricStore::new(600, Some(persistent.clone())).persist_tiers(&[Tier::Minute, Tier::Hour]);

    // Ingest: one sample per series per minute, through the real seam. A
    // per-second cadence would measure the hot ring, which is memory and not
    // what a disk bench is for.
    let ingest = Instant::now();
    let base_ms = 1_700_000_000_000i64;
    for minute in 0..args.minutes {
        let ts = base_ms + minute * 60_000;
        for i in 0..args.series {
            let origin = format!("h-{:012x}", i % 6); // six hosts, as the fleet has
            let subject = format!("bench/series_{i}");
            let point = TelemetryPoint {
                timestamp: ts,
                source: format!("vm-{}", i % 6),
                protocol: Protocol::Sysinfo,
                metric: subject.clone(),
                // A counter: the kind that costs most to answer, since `rate`
                // has to walk the series rather than reduce it.
                value: TelemetryValue::Counter((minute as u64) * 1_000 + i as u64),
                labels: Default::default(),
                unit: Some("By".into()),
            };
            store.record(&origin, &subject, &point);
        }
        // Flush per simulated minute, as the historian's timer would.
        if let Some((handle, batch)) = store.take_flush_batch() {
            handle.write_batch(&batch)?;
        }
    }
    let ingest_s = ingest.elapsed().as_secs_f64();

    let interned = store.interner().len() as u64;
    // Captured before `store` is dropped for the compaction step below.
    let ids_for_query: Vec<_> = store
        .interner()
        .with_prefix("")
        .map(|(id, _)| id)
        .take(200)
        .collect();
    let second_rows = persistent.tier_rows(Tier::Second)?;
    let minute_rows = persistent.tier_rows(Tier::Minute)?;
    let hour_rows = persistent.tier_rows(Tier::Hour)?;
    let db_bytes = persistent.db_bytes();
    // Every tier, so bytes-per-bucket divides by what is actually on disk.
    // Counting only two of three is how the first run of this bench reported
    // 438 B/bucket for a file that held 222.
    let total_rows = second_rows + minute_rows + hour_rows;

    println!("## Ingest");
    println!("interned series:     {interned}");
    println!("second buckets:      {second_rows}  (0 = the ring is not flushed)");
    println!("minute buckets:      {minute_rows}");
    println!("hour buckets:        {hour_rows}");
    println!("database bytes:      {db_bytes}");
    if total_rows > 0 {
        println!(
            "bytes per bucket:    {:.1}",
            db_bytes as f64 / total_rows as f64
        );
    }
    println!("ingest wall time:    {ingest_s:.1}s");
    println!();

    // Prune: the pass that keeps the file bounded. `now` far enough ahead that
    // everything written has aged past the minute tier's retention, so this is
    // the worst case rather than the steady-state handful.
    let prune_at = base_ms + args.minutes * 60_000 + Tier::Minute.retention_secs() * 1_000 + 1_000;
    let pruned = Instant::now();
    let removed = persistent.prune(prune_at)?;
    let prune_ms = pruned.elapsed().as_millis();
    if args.ingest_only {
        // Close both handles so the file is consistent for another process to
        // open. Copying an OPEN redb file is not a snapshot — the first
        // attempt at this read one and concluded the minute tier was missing,
        // which it was not.
        drop(store);
        drop(persistent);
        println!(
            "(stopped after ingest; {} is closed and consistent)",
            args.path.display()
        );
        return Ok(());
    }

    // NOT MEASURED: compaction. `redb::Database::compact` reclaims slack and
    // the difference looked dramatic — but the figure could not be reconciled
    // with an independent reader of the same file, and a number nobody can
    // reconcile is not a measurement. See the note in docs/storage.md; it
    // needs settling before anyone tunes a retention default on the strength
    // of a bytes-per-bucket figure.

    println!("## Prune (worst case: every minute bucket aged out at once)");
    println!("removed:             {removed}");
    println!("wall time:           {prune_ms}ms");
    println!();

    // Query: a 24 h range at the minute tier, the shape a device chart asks
    // for. Latency over enough series to see a tail rather than one sample.
    let ids: Vec<_> = ids_for_query;
    let mut latencies_us: Vec<u128> = Vec::with_capacity(ids.len());
    for id in &ids {
        let t = Instant::now();
        let _ = persistent.query_buckets(*id, Tier::Hour, base_ms, prune_at)?;
        latencies_us.push(t.elapsed().as_micros());
    }
    latencies_us.sort_unstable();
    println!(
        "## Range query (hour tier, full span, {} series)",
        ids.len()
    );
    if !latencies_us.is_empty() {
        let p = |q: f64| latencies_us[((latencies_us.len() as f64 - 1.0) * q) as usize];
        println!("p50:                 {:.2}ms", p(0.50) as f64 / 1000.0);
        println!("p95:                 {:.2}ms", p(0.95) as f64 / 1000.0);
        println!("max:                 {:.2}ms", p(1.0) as f64 / 1000.0);
    }
    println!();

    println!("## Not measured here");
    println!("Steady RSS under the governor's ladder. It depends on how many");
    println!("series are ACTIVE at once and how fast they arrive, not on how");
    println!("many exist — a property of the running service under a real");
    println!("fleet's arrival pattern, which no bench reproduces. #911's soak");
    println!("is what answers it.");

    if !args.keep {
        let _ = std::fs::remove_file(&args.path);
    } else {
        println!();
        println!("(kept {})", args.path.display());
    }
    Ok(())
}
