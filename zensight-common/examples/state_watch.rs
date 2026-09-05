//! Watch a state selector over time and write one NDJSON line per sample.
//!
//! The third reader in this pair, and the one the other two could not be.
//! [`rpc_get`] issues ONE GET and exits — which answers "what does the bus hold
//! right now", and only for keys something actually serves. [`v1_probe`]
//! listens and waits for sensors to dial *in*, so it is a hub and cannot be
//! pointed at a fleet that already exists. Neither can answer "what did this
//! key do over the next fortnight", which is what sizing (#944) needs.
//!
//! ```bash
//! WATCH_SECS=600 PROBE_CONNECT=tcp/127.0.0.1:7447 \
//!     cargo run -p zensight-common --example state_watch -- 'v1/*/state/*/health'
//! ```
//!
//! **Why a subscriber and not a GET loop.** The health document has no
//! late-joiner seed and does not need one: the runner republishes it every five
//! seconds, so a subscriber converges in five seconds while a GET on the same
//! selector answers nothing at all. Polling it would have measured whichever
//! instants the poller happened to wake on; subscribing sees every tick.
//!
//! **Why the per-key throttle.** Health at 5 s is 17 280 documents per sensor
//! per day, so #944's fourteen-day window on six VMs is a couple of gigabytes of
//! JSON to answer a question about a few dozen numbers.
//! `WATCH_MIN_INTERVAL_SECS` (default 60) keeps at most one sample per key per
//! interval. It throttles per KEY, not globally, so a fleet's rarest publisher
//! is never crowded out by its noisiest — which is the failure mode of every
//! "keep the last N samples" alternative.
//!
//! Output is NDJSON, one object per line: `{"t":<epoch ms>,"key":…,"v":…}`, so a
//! run can be tailed, truncated, or fed to a reader line by line. A payload that
//! decodes as neither JSON nor CBOR is reported with its length rather than
//! dropped — a sample that arrived and could not be read is evidence, and it is
//! a different fact from a sample that never came.
//!
//! Exits non-zero when nothing arrived, so "the selector is wrong" and "the
//! fleet is quiet" do not both look like success.
//!
//! ```bash
//! # env
//! #   PROBE_CONNECT            endpoint to dial   (default tcp/127.0.0.1:17447)
//! #   WATCH_SECS               how long to watch  (default 60; 0 = until SIGINT)
//! #   WATCH_MIN_INTERVAL_SECS  per-key throttle   (default 60; 0 = every sample)
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let Some(selector) = std::env::args().nth(1) else {
        eprintln!(
            "usage: state_watch <selector>   e.g. 'v1/*/state/*/health'\n\
             env: PROBE_CONNECT (default tcp/127.0.0.1:17447), WATCH_SECS (60), \
             WATCH_MIN_INTERVAL_SECS (60)"
        );
        std::process::exit(2);
    };
    let connect = std::env::var("PROBE_CONNECT").unwrap_or_else(|_| "tcp/127.0.0.1:17447".into());
    let secs: u64 = env_num("WATCH_SECS", 60);
    let min_interval_ms: i64 = env_num("WATCH_MIN_INTERVAL_SECS", 60) as i64 * 1000;

    // A CLIENT, not a peer — the same reasoning as `rpc_get`: a peer with
    // gossip off knows only the endpoint it dialled, and this has to see a
    // whole fleet's publishers, including the ones a hop behind the router.
    let mut config = zenoh::Config::default();
    config.insert_json5("mode", "\"client\"").unwrap();
    // Scouting fully off: a sizing run must observe the deployment it was
    // pointed at and never some other fleet that happens to share the LAN.
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
        .insert_json5("connect/endpoints", &format!("[\"{connect}\"]"))
        .unwrap();
    let session = zenoh::open(config).await.expect("open session");

    let subscriber = session
        .declare_subscriber(&selector)
        .await
        .expect("declare subscriber");

    eprintln!(
        "watching {selector} on {connect} for {}",
        if secs == 0 {
            "ever (Ctrl-C to stop)".to_string()
        } else {
            format!("{secs}s")
        }
    );

    let deadline = if secs == 0 {
        None
    } else {
        Some(tokio::time::Instant::now() + Duration::from_secs(secs))
    };

    let mut last_emit: HashMap<String, i64> = HashMap::new();
    let mut seen = 0u64;
    let mut written = 0u64;
    let stdout = std::io::stdout();

    loop {
        let sample = match deadline {
            Some(d) => match tokio::time::timeout_at(d, subscriber.recv_async()).await {
                Err(_) => break,
                Ok(Err(_)) => break,
                Ok(Ok(s)) => s,
            },
            None => match subscriber.recv_async().await {
                Err(_) => break,
                Ok(s) => s,
            },
        };

        seen += 1;
        let key = sample.key_expr().to_string();
        let now = zensight_common::current_timestamp_millis();
        if min_interval_ms > 0
            && let Some(prev) = last_emit.get(&key)
            && now - prev < min_interval_ms
        {
            continue;
        }
        last_emit.insert(key.clone(), now);

        let bytes = sample.payload().to_bytes();
        let value = match zensight_common::decode_auto::<serde_json::Value>(&bytes) {
            Ok(v) => v,
            // Undecodable is a fact about the sample, not a reason to lose it.
            Err(e) => serde_json::json!({
                "_undecodable": e.to_string(),
                "_bytes": bytes.len(),
            }),
        };
        let line = serde_json::json!({ "t": now, "key": key, "v": value });
        let mut out = stdout.lock();
        if writeln!(out, "{line}").is_err() {
            // A closed stdout (`| head`) is an ordinary end, not a failure.
            break;
        }
        let _ = out.flush();
        written += 1;
    }

    eprintln!(
        "{seen} samples seen, {written} written ({} distinct keys) for {selector}",
        last_emit.len()
    );
    // Nothing arriving is the failure this exists to make visible: a wrong
    // selector, a wrong endpoint and a silent fleet all produce zero lines, and
    // a zero-length file is not evidence of a fleet that uses no memory.
    std::process::exit(if written > 0 { 0 } else { 1 });
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
