//! Record every `@media` sample of one stream tier to a CSV, and nothing else.
//!
//! This is the instrument for #713 — "what does `@media` loss actually look
//! like?". Every knob in epic #712 (the frame-age deadline, the report's loss
//! field, the controller's downgrade threshold) is a number tuned against an
//! assumed loss distribution, and the assumption has never been checked. It
//! cannot be checked from inside the GUI, because the GUI *reacts*: it sheds,
//! it resyncs, it asks for keyframes. A measurement has to be the one consumer
//! that does none of that.
//!
//! So the probe subscribes, writes one row per sample, and never decodes,
//! never sheds, never asks for a keyframe. What it costs is realism — a real
//! viewer would have recovered — and what it buys is that the CSV is the wire,
//! not the wire plus a policy.
//!
//! Analysis is a *separate* script (`scripts/media-loss-report.py`), so a run
//! that took ten minutes behind a netem qdisc is re-analysable without
//! re-running it. That split is the reason this writes rows rather than
//! verdicts.
//!
//! ```text
//! cargo run -p zensight-sensor-parallax --example media_loss_probe --release -- \
//!     --origin h-3fa9c2d41b7e --stream test0 --tier high \
//!     --connect 'quic/10.77.0.1:7447?mixed_rel=1' --root-ca /tmp/lab/ca.crt \
//!     --seconds 60 --out /tmp/lab/quic-1pct.csv
//! ```
//!
//! It builds its own `zenoh::Config` rather than going through
//! [`zensight_common::session`]. That is sanctioned for exactly this — the
//! `build_config` doc comment names "the isolated-run configs … and the e2e
//! tests" — and it is *necessary* here: the probe is the QUIC **listener** in
//! the lab, and the shared config sets connect-side TLS material only, because
//! in a real deployment the listening side is the zenohd router. The sensor
//! under measurement keeps its normal config and stays the connect side.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use clap::Parser;
use zensight_common::keyexpr::{
    media_preview_key, media_video_key, origin_rpc_key, origin_telemetry_wildcard,
};
use zensight_common::media::observed_frame_age_ms;
use zensight_common::stream::{FrameMeta, StreamControl};
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};
use zensight_common::{Format, decode, decode_auto};

#[derive(Parser, Debug)]
#[command(about = "Record one @media tier's samples to CSV (#713)")]
struct Args {
    /// The publishing host's origin, e.g. `h-3fa9c2d41b7e`. No wildcard: RFC 07
    /// §3 forbids fanning the bulk planes across origins.
    #[arg(long)]
    origin: String,
    /// Stream name (the `<stream>` key chunk).
    #[arg(long)]
    stream: String,
    /// Tier to subscribe to. Ignored with `--preview`.
    #[arg(long, default_value = "high")]
    tier: String,
    /// Measure the JPEG preview key instead of a video tier.
    #[arg(long)]
    preview: bool,
    /// Where to write the CSV.
    #[arg(long)]
    out: String,
    /// Where to write the sender's own stats, one row per telemetry point.
    ///
    /// Defaults to `<out>`'s directory. This is not optional decoration: the
    /// receiver's CSV cannot tell a frame the *wire* lost from a frame the
    /// encoder never handed to the wire, and `stats/drops` (AppSink shedding
    /// under a slow consumer) and `stats/rc_drops` (the rate controller
    /// skipping frames) are the only numbers that can.
    #[arg(long)]
    stats_out: Option<String>,
    /// How long to record for.
    #[arg(long, default_value_t = 30)]
    seconds: u64,

    /// Zenoh mode.
    #[arg(long, default_value = "peer")]
    mode: String,
    /// Endpoints to dial.
    #[arg(long)]
    connect: Vec<String>,
    /// Endpoints to listen on.
    #[arg(long)]
    listen: Vec<String>,
    /// CA that signed the peer's certificate (`tls/`, `quic/`).
    #[arg(long)]
    root_ca: Option<String>,
    /// This process's own certificate, when it is the `quic/`/`tls/` listener.
    #[arg(long)]
    listen_cert: Option<String>,
    /// The private key for `--listen-cert`.
    #[arg(long)]
    listen_key: Option<String>,

    /// How long to wait for the sensor to become reachable before giving up.
    ///
    /// The lab starts the sensor first and the probe second, but the probe is
    /// the *listener*, so the sensor's first dials fail and zenoh retries them
    /// in the background. Commanding a peer that has not linked yet gets no
    /// reply at all, not a refusal, so this retries rather than reporting a
    /// stream that was never opened as one that lost every frame.
    #[arg(long, default_value_t = 30)]
    wait_secs: u64,
    /// Ask the sensor to open the tier before subscribing, and close it after.
    ///
    /// On by default because a parallax profile is demand-driven: no encoder
    /// runs until something opens it, and the matching listener that keeps it
    /// running only sees an edge once a publisher exists to match against.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    open: bool,
}

/// One row of the CSV. Deliberately raw: no rate, no loss, no verdict.
///
/// `age_ms` is empty rather than `0` when the sample arrived unstamped —
/// RFC 07 §1.3's rule, and the one place a measurement is most tempted to
/// break it, since an empty column is inconvenient and a zero is not.
fn write_row(
    out: &mut impl Write,
    recv_ns: u128,
    meta: Option<&FrameMeta>,
    bytes: usize,
    age_ms: Option<f64>,
) -> Result<()> {
    let (seq, key, w, h) = match meta {
        Some(m) => (
            m.sequence.to_string(),
            u8::from(m.keyframe).to_string(),
            m.width.to_string(),
            m.height.to_string(),
        ),
        // A sample whose attachment did not decode is still a sample that
        // arrived, and dropping the row would quietly shrink the denominator.
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    let age = age_ms.map(|a| format!("{a:.3}")).unwrap_or_default();
    writeln!(out, "{recv_ns},{seq},{key},{bytes},{age},{w},{h}")?;
    Ok(())
}

fn build_config(args: &Args) -> Result<zenoh::Config> {
    let mut config = zenoh::Config::default();
    let set = |config: &mut zenoh::Config, k: &str, v: &str| -> Result<()> {
        config
            .insert_json5(k, v)
            .map_err(|e| anyhow::anyhow!("zenoh config {k}: {e}"))
    };
    set(&mut config, "mode", &format!("{:?}", args.mode))?;
    // Stamping is what makes `frame_age_ms` measurable at all (RFC 07 §1.3);
    // zenoh turns it on by default only for routers.
    set(&mut config, "timestamping/enabled", "true")?;
    // The lab is a two-node namespace pair joined by one veth. Discovery has
    // nothing to find and multicast across a netns boundary is a distraction.
    set(&mut config, "scouting/multicast/enabled", "false")?;
    set(&mut config, "scouting/gossip/enabled", "false")?;
    if !args.connect.is_empty() {
        set(
            &mut config,
            "connect/endpoints",
            &serde_json::to_string(&args.connect)?,
        )?;
    }
    if !args.listen.is_empty() {
        set(
            &mut config,
            "listen/endpoints",
            &serde_json::to_string(&args.listen)?,
        )?;
    }
    // QUIC links read the same `transport/link/tls` block as `tls/` links.
    for (k, v) in [
        ("root_ca_certificate", &args.root_ca),
        ("listen_certificate", &args.listen_cert),
        ("listen_private_key", &args.listen_key),
    ] {
        if let Some(path) = v {
            set(
                &mut config,
                &format!("transport/link/tls/{k}"),
                &format!("{path:?}"),
            )?;
        }
    }
    Ok(config)
}

/// Send one `stream/set` command and return the sensor's answer as text.
async fn control(
    session: &zenoh::Session,
    origin: &zenkey::RemoteOrigin,
    body: StreamControl,
) -> Result<()> {
    let key = origin_rpc_key(origin, "parallax", "stream/set");
    let cmd = zensight_common::command::Command::new(body);
    let replies = session
        .get(&key)
        .payload(serde_json::to_vec(&cmd)?)
        .target(zenoh::query::QueryTarget::All)
        .timeout(Duration::from_secs(5))
        .await
        .map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
    let reply = replies
        .recv_async()
        .await
        .map_err(|e| anyhow::anyhow!("no reply on {key}: {e}"))?;
    match reply.result() {
        Ok(_) => Ok(()),
        Err(e) => bail!(
            "refused: {}",
            String::from_utf8_lossy(&e.payload().to_bytes())
        ),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();
    let origin = zenkey::RemoteOrigin::parse(&args.origin)
        .map_err(|e| anyhow::anyhow!("--origin {}: {e}", args.origin))?;

    let session = zenoh::open(build_config(&args)?)
        .await
        .map_err(|e| anyhow::anyhow!("zenoh open: {e}"))?;

    let (codec, key) = if args.preview {
        ("mjpeg", media_preview_key(&origin, &args.stream))
    } else {
        (
            "h264",
            media_video_key(&origin, &args.stream, "h264", &args.tier),
        )
    };
    let tier = (!args.preview).then(|| args.tier.clone());

    if args.open {
        let open = StreamControl::OpenStream {
            stream: args.stream.clone(),
            codec: Some(codec.to_string()),
            tier: tier.clone(),
        };
        let deadline = Instant::now() + Duration::from_secs(args.wait_secs);
        loop {
            match control(&session, &origin, open.clone()).await {
                Ok(()) => break,
                Err(e) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = e;
                }
                Err(e) => return Err(e).context("opening the stream"),
            }
        }
    }

    let subscriber = session
        .declare_subscriber(&key)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe {key}: {e}"))?;

    let mut out = std::io::BufWriter::new(
        std::fs::File::create(&args.out).with_context(|| format!("creating {}", args.out))?,
    );
    writeln!(out, "recv_ns,seq,keyframe,bytes,age_ms,width,height")?;

    // The sender's own account of the same seconds. `origin_telemetry_wildcard`
    // is one host's whole telemetry tree; the filter below narrows it to this
    // stream's stats without inventing a key spelling the sensor does not use.
    let stats_key = origin_telemetry_wildcard(&args.origin);
    let stats_sub = session
        .declare_subscriber(&stats_key)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe {stats_key}: {e}"))?;
    let stats_path = args.stats_out.clone().unwrap_or_else(|| {
        let p = std::path::Path::new(&args.out);
        p.with_file_name("sender-stats.csv").display().to_string()
    });
    let mut stats_out = std::io::BufWriter::new(
        std::fs::File::create(&stats_path).with_context(|| format!("creating {stats_path}"))?,
    );
    writeln!(stats_out, "recv_ns,metric,value")?;

    let started = Instant::now();
    let window = Duration::from_secs(args.seconds);
    let mut rows: u64 = 0;
    let mut stat_rows: u64 = 0;
    let stats_prefix = format!("/{}/stats/", args.stream);
    loop {
        let left = match window.checked_sub(started.elapsed()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };
        tokio::select! {
            biased;
            frame = tokio::time::timeout(left, subscriber.recv_async()) => {
                let sample = match frame {
                    Ok(Ok(s)) => s,
                    // The publisher went away; a run that recorded nothing is
                    // still a result, so stop rather than fail.
                    Ok(Err(_)) | Err(_) => break,
                };
                let meta: Option<FrameMeta> = sample
                    .attachment()
                    .and_then(|a| decode(&a.to_bytes(), Format::Cbor).ok());
                let age = observed_frame_age_ms(sample.timestamp(), SystemTime::now());
                write_row(
                    &mut out,
                    started.elapsed().as_nanos(),
                    meta.as_ref(),
                    sample.payload().len(),
                    age,
                )?;
                rows += 1;
            }
            point = stats_sub.recv_async() => {
                let Ok(sample) = point else { continue };
                if !sample.key_expr().as_str().contains(&stats_prefix) {
                    continue;
                }
                let Ok(p) = decode_auto::<TelemetryPoint>(&sample.payload().to_bytes()) else {
                    continue;
                };
                let value = match p.value {
                    TelemetryValue::Counter(c) => c as f64,
                    TelemetryValue::Gauge(g) => g,
                    // Nothing else appears under `stats/`, and a stats row that
                    // is not a number is not one this analysis can use.
                    _ => continue,
                };
                writeln!(
                    stats_out,
                    "{},{},{value}",
                    started.elapsed().as_nanos(),
                    p.metric
                )?;
                stat_rows += 1;
            }
        }
    }
    out.flush()?;
    stats_out.flush()?;

    if args.open {
        // Best effort: the run's numbers are already on disk, and a sensor that
        // has gone away is not a reason to fail a completed measurement.
        if let Err(e) = control(
            &session,
            &origin,
            StreamControl::CloseStream {
                stream: args.stream.clone(),
                codec: Some(codec.to_string()),
                tier,
            },
        )
        .await
        {
            eprintln!("close failed (harmless, idle timeout will reap): {e}");
        }
    }
    eprintln!("{rows} samples -> {}", args.out);
    eprintln!("{stat_rows} sender stats -> {stats_path}");
    Ok(())
}
