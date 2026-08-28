//! One-shot `@rpc` GET — read a queryable without a GUI (#168).
//!
//! `zenctl` lives in the external zenkey repo and the GUI needs a display, so
//! on-host validation of a query channel had no reader at all. This is that
//! reader: one GET, every reply printed as pretty JSON, exit non-zero when
//! nobody answered.
//!
//! Unlike [`v1_probe`], which listens and waits for sensors to dial in, this
//! **connects** — a validation run starts the sensor first, so dialling an
//! already-listening peer avoids sitting out a connect-retry backoff.
//!
//! ```bash
//! PROBE_CONNECT=tcp/127.0.0.1:17447 \
//!     cargo run -p zensight-common --example rpc_get -- 'v1/*/@rpc/sysinfo/latency'
//! ```
//!
//! Start the sensor with `ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:17447
//! ZENSIGHT_ZENOH_SCOUTING=false`.
//!
//! The selector is a full wire key (a debug tool sees the un-namespaced wire,
//! RFC 09 §5), so it starts at `v1/` on a base-less deployment. Every `@rpc`
//! reply on the sensors goes out through `reply_json`, so plain JSON decoding
//! is right — there is no CBOR sniff to do.

use std::time::Duration;

#[tokio::main]
async fn main() {
    let Some(selector) = std::env::args().nth(1) else {
        eprintln!(
            "usage: rpc_get <selector>   e.g. 'v1/*/@rpc/sysinfo/latency'\n\
             env: PROBE_CONNECT (default tcp/127.0.0.1:17447), PROBE_TIMEOUT_SECS (5)"
        );
        std::process::exit(2);
    };
    let connect = std::env::var("PROBE_CONNECT").unwrap_or_else(|_| "tcp/127.0.0.1:17447".into());
    let timeout: u64 = std::env::var("PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let mut config = zenoh::Config::default();
    // Scouting fully off: this dials exactly one endpoint and joins nothing
    // else, so a validation run cannot accidentally answer from some other
    // sensor on the LAN.
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
    // Let the TCP link come up before the GET; a query issued into a
    // half-open session matches no queryable and looks like "nobody answered".
    tokio::time::sleep(Duration::from_millis(500)).await;

    let replies = session
        .get(&selector)
        .target(zenoh::query::QueryTarget::All)
        .timeout(Duration::from_secs(timeout))
        .await
        .expect("send query");

    let mut answered = 0usize;
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => {
                answered += 1;
                let bytes = sample.payload().to_bytes();
                match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(v) => println!(
                        "{}\n{}",
                        sample.key_expr(),
                        serde_json::to_string_pretty(&v).unwrap()
                    ),
                    Err(_) => println!("{} => {} bytes, not JSON", sample.key_expr(), bytes.len()),
                }
            }
            // An RPC error reply is an answer too — print it and keep going.
            Err(e) => {
                answered += 1;
                println!("ERR {e:?}");
            }
        }
    }

    eprintln!("{answered} replies for {selector}");
    std::process::exit(if answered > 0 { 0 } else { 1 });
}
