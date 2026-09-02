//! A one-shot `@rpc/historian/*` client, for scripts that must *ask* rather
//! than compile (#912).
//!
//! `scripts/demo-verify.sh` executes the historian and then queries it, for
//! the same reason it executes the exporters and then scrapes them (#845):
//! compiling a query path proves nothing about it. The crate's own round trips
//! use an in-process session, which is the right shape for a unit test and the
//! wrong one for a smoke test — it never starts the real binary, never crosses
//! a real socket, and never reads a config.
//!
//! `zenctl` would do this, and is deliberately not used: it is an external
//! crate from another repository, so a CI job that depended on it would be
//! testing whether that tool was installed. Forty lines here depend on
//! nothing the workspace does not already build.
//!
//! ```text
//! cargo run -p zensight-historian --example historian-query -- \
//!     --connect tcp/127.0.0.1:17447 'v1/*/@rpc/historian/series?producer=sysinfo'
//! ```
//!
//! Prints each reply's JSON, one per line. Exit 0 when at least one value
//! reply arrived, 1 on an error reply, 2 on silence — the three states RFC 05
//! §3.1 insists are different.

use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut connect: Vec<String> = Vec::new();
    let mut selector = None;
    let mut timeout_s = 10u64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--connect" | "-c" => connect.push(
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("--connect needs an endpoint"))?,
            ),
            "--timeout" => {
                timeout_s = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--timeout needs seconds"))?
                    .parse()?
            }
            other => selector = Some(other.to_string()),
        }
    }
    let selector = selector
        .ok_or_else(|| anyhow::anyhow!("usage: historian-query [-c ENDPOINT] <SELECTOR>"))?;

    // Through the sanctioned seam, like every other client: it is the only
    // place the deployment namespace and timestamping are set, and a session
    // opened around it would silently see a different bus.
    let config = zensight_common::config::ZenohConfig {
        mode: "client".to_string(),
        connect: connect.clone(),
        scouting: Some(false),
        gossip: Some(false),
        ..Default::default()
    };
    let session = zensight_common::session::connect(&config).await?;

    // Target All with consolidation off — the fan-in discipline (RFC 05 §2.1).
    // Several historians may answer, each on its own concrete key, and
    // BestMatching would take whichever replied first.
    let replies = session
        .get(&selector)
        .target(zenoh::query::QueryTarget::All)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .timeout(Duration::from_secs(timeout_s))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let (mut values, mut errors) = (0usize, 0usize);
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => {
                values += 1;
                println!("{}", String::from_utf8_lossy(&sample.payload().to_bytes()));
            }
            Err(e) => {
                errors += 1;
                eprintln!(
                    "error reply: {}",
                    String::from_utf8_lossy(&e.payload().to_bytes())
                );
            }
        }
    }
    session.close().await.map_err(|e| anyhow::anyhow!("{e}"))?;

    // Silence, an error and an answer are three different things, and a script
    // that collapsed them would report a dead historian as an empty one.
    if values > 0 {
        Ok(())
    } else if errors > 0 {
        std::process::exit(1);
    } else {
        eprintln!("no replies to {selector:?} within {timeout_s}s");
        std::process::exit(2);
    }
}
