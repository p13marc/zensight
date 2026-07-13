//! `zenctl` — a bus explorer for the keyspace-v2 convention.
//!
//! RFC 08 §6 names this tool: runtime introspection exists so that "generic
//! explorer tooling — the `busctl`/`d-feet` equivalent — needs no compiled-in
//! registry". Every producer MUST serve `@rpc/<producer>/introspect`, so a
//! fleet can describe itself.
//!
//! The commands split in two:
//!
//! * **offline** (`topic list`, `topic info`, `interface show`, `service list`)
//!   answer from the registry this binary compiled against. They work with the
//!   fleet down, and they describe what *may* exist.
//! * **on-bus** (`node list`, `topic echo`, `service call`, `doctor`) ask the
//!   fleet. They describe what *does* exist.
//!
//! Keeping the two visibly distinct is deliberate: the gap between them is
//! exactly where drift lives, and `doctor` is the command that reports it.

mod bus;
mod offline;

use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "zenctl",
    about = "Explore a ZenSight keyspace-v2 bus (RFC 08 §6)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subjects: what data exists, and what it means.
    #[command(subcommand)]
    Topic(TopicCmd),
    /// Producers: who is alive on the bus.
    #[command(subcommand)]
    Node(NodeCmd),
    /// Procedures on the `@rpc` plane.
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Payload types (the RFC 08 §5 type table).
    #[command(subcommand)]
    Interface(InterfaceCmd),
    /// Diff what the fleet *serves* against what this build *expects*.
    ///
    /// RFC 08 §6: "A disagreement between introspection and the checked-in TOML
    /// is a finding, not an ambiguity." This prints the findings.
    Doctor(BusArgs),
}

#[derive(Subcommand)]
enum TopicCmd {
    /// List registered subjects (offline — from the compiled-in registry).
    List {
        /// Only this producer.
        #[arg(long)]
        producer: Option<String>,
        /// Only this class: telemetry, state, or events.
        #[arg(long)]
        class: Option<String>,
    },
    /// Describe one key or subject pattern (offline).
    ///
    /// Accepts a full wire key (`zensight/@v1/h-abc.../telemetry/sysinfo/cpu/usage`)
    /// and refines it through the registry's parse direction.
    Info {
        /// A concrete wire key, as it appears on the bus.
        key: String,
    },
    /// Subscribe and print decoded samples (on-bus).
    Echo {
        /// Key expression to subscribe to. Defaults to all v1 data.
        #[arg(default_value = "zensight/@v1/**")]
        selector: String,
        /// Print raw payload bytes as hex instead of decoding.
        #[arg(long)]
        raw: bool,
        /// Stop after this many samples (0 = run until interrupted).
        #[arg(long, default_value_t = 0)]
        count: usize,
        #[command(flatten)]
        bus: BusArgs,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    /// List live producers from the liveliness roster (on-bus).
    List(BusArgs),
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// List registered procedures (offline).
    List {
        /// Only this producer.
        #[arg(long)]
        producer: Option<String>,
    },
    /// Call a procedure (on-bus).
    Call {
        /// Origin to target: a host id (`h-3fa9c2d41b7e`), `*` for the whole
        /// fleet, or `@catalog` for the identity service.
        origin: String,
        /// Producer name. Omit for a service origin, which has no producer chunk.
        producer: String,
        /// Procedure path, e.g. `introspect` or `artifact/status`.
        procedure: String,
        /// Selector parameters, repeatable: `--param state=established`.
        #[arg(long = "param", value_name = "K=V")]
        params: Vec<String>,
        /// Request body: inline JSON, or `@path` to read a file.
        #[arg(long)]
        body: Option<String>,
        #[command(flatten)]
        bus: BusArgs,
    },
}

#[derive(Subcommand)]
enum InterfaceCmd {
    /// List every payload type in the type table (offline).
    List,
    /// Show one payload type and where it is defined (offline).
    Show {
        /// Type name, e.g. `TelemetryPoint`.
        type_name: String,
    },
}

/// Connection options shared by every on-bus command.
#[derive(Args, Clone)]
struct BusArgs {
    /// Endpoint to connect to, repeatable (e.g. `tcp/127.0.0.1:7447`).
    #[arg(long, short = 'c')]
    connect: Vec<String>,
    /// Endpoint to listen on, repeatable.
    #[arg(long, short = 'l')]
    listen: Vec<String>,
    /// Enable multicast scouting.
    ///
    /// OFF by default, and you should think before turning it on: a scouting
    /// explorer joins whatever mesh it can find, which is how a throwaway
    /// session ends up talking to a production fleet.
    #[arg(long)]
    scouting: bool,
    /// Seconds to wait for replies / to watch the bus.
    #[arg(long, default_value_t = 5)]
    timeout: u64,
}

impl BusArgs {
    async fn session(&self) -> Result<zenoh::Session> {
        bus::open(&self.connect, &self.listen, self.scouting).await
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Topic(TopicCmd::List { producer, class }) => {
            offline::topic_list(producer.as_deref(), class.as_deref())
        }
        Command::Topic(TopicCmd::Info { key }) => offline::topic_info(&key),
        Command::Topic(TopicCmd::Echo {
            selector,
            raw,
            count,
            bus,
        }) => cmd_echo(&selector, raw, count, &bus).await,
        Command::Node(NodeCmd::List(bus)) => cmd_node_list(&bus).await,
        Command::Service(ServiceCmd::List { producer }) => {
            offline::service_list(producer.as_deref())
        }
        Command::Service(ServiceCmd::Call {
            origin,
            producer,
            procedure,
            params,
            body,
            bus,
        }) => {
            cmd_service_call(
                &origin,
                &producer,
                &procedure,
                &params,
                body.as_deref(),
                &bus,
            )
            .await
        }
        Command::Interface(InterfaceCmd::List) => offline::interface_list(),
        Command::Interface(InterfaceCmd::Show { type_name }) => offline::interface_show(&type_name),
        Command::Doctor(bus) => cmd_doctor(&bus).await,
    }
}

/// `node list` — the liveliness roster (RFC 04 §5).
async fn cmd_node_list(args: &BusArgs) -> Result<()> {
    let session = args.session().await?;
    let roster = bus::roster(&session, args.timeout()).await?;

    if roster.is_empty() {
        println!("no live producers.");
        println!(
            "\nnothing held a liveliness token on {}.",
            zensight_common::all_liveliness_wildcard()
        );
        println!("if you expected some, check --connect (and that they are actually running).");
        return Ok(());
    }

    for (origin, producers) in &roster {
        println!("{origin}");
        for p in producers {
            println!("  {p}");
        }
    }
    let total: usize = roster.values().map(Vec::len).sum();
    println!("\n{total} producer(s) on {} origin(s).", roster.len());
    Ok(())
}

/// `topic echo` — subscribe, refine each key through the registry, decode.
///
/// Subscribe-first is not a style choice: RFC 04 §3.2 forbids GET-then-subscribe
/// (it drops everything published in the gap). This only subscribes, so it is
/// trivially correct — but a `--seed` flag would have to keep that order.
async fn cmd_echo(selector: &str, raw: bool, count: usize, args: &BusArgs) -> Result<()> {
    let session = args.session().await?;
    let subscriber = session
        .declare_subscriber(selector)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("echoing {selector} (ctrl-c to stop)");
    let mut seen = 0usize;
    while let Ok(sample) = subscriber.recv_async().await {
        let key = sample.key_expr().as_str().to_string();
        let bytes = sample.payload().to_bytes();

        if raw {
            println!("{key}\n  {}", hex(&bytes));
        } else {
            match render(&key, &bytes) {
                Ok(line) => println!("{key}\n  {line}"),
                Err(why) => println!("{key}\n  <{why}>"),
            }
        }

        seen += 1;
        if count > 0 && seen >= count {
            break;
        }
    }
    Ok(())
}

/// Wire key → subject → payload type → value, with nothing producer-specific
/// compiled in. This is the whole point of the registry's parse direction.
fn render(key: &str, bytes: &[u8]) -> std::result::Result<String, String> {
    let (_, _, subject) = zensight_common::keyexpr::refine_wire_key(key)
        .ok_or_else(|| "unregistered subject — not in this build's registry".to_string())?;
    let value = zensight_common::decode_payload(subject.payload_type(), bytes)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string(&value).unwrap_or_default())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `service call` — a GET on the `@rpc` plane (RFC 05).
async fn cmd_service_call(
    origin: &str,
    producer: &str,
    procedure: &str,
    params: &[String],
    body: Option<&str>,
    args: &BusArgs,
) -> Result<()> {
    // A service origin (`@catalog`) carries no producer chunk (RFC 03 §1.5).
    let mut key = if origin.starts_with('@') {
        format!("zensight/@v1/{origin}/@rpc/{procedure}")
    } else {
        format!("zensight/@v1/{origin}/@rpc/{producer}/{procedure}")
    };
    if !params.is_empty() {
        key.push('?');
        key.push_str(&params.join(";"));
    }

    let payload = match body {
        Some(b) => Some(match b.strip_prefix('@') {
            Some(path) => std::fs::read(path)?,
            None => b.as_bytes().to_vec(),
        }),
        None => None,
    };

    let session = args.session().await?;
    eprintln!("GET {key}");
    let answers = bus::fleet_get(&session, &key, payload, args.timeout()).await?;

    if answers.is_empty() {
        // RFC 05 §3.1: "no reply" is not one condition, and callers "MUST NOT
        // treat 'empty reply set' as a verdict about any specific host".
        println!("no replies.");
        println!("\nthat is not a verdict: an empty set means no queryable matched — an offline");
        println!("host, a mistyped origin, or a procedure this producer does not serve.");
        println!("`zenctl node list` says who is actually up.");
        return Ok(());
    }

    for FleetAnswerRef { origin, answer } in answers.iter().map(|a| FleetAnswerRef {
        origin: &a.origin,
        answer: &a.answer,
    }) {
        match answer {
            bus::Answer::Value(bytes) => {
                // Read replies are JSON today; introspect replies raw TOML.
                // Print what parses, fall back to the text.
                let text = match serde_json::from_slice::<serde_json::Value>(bytes) {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                    Err(_) => String::from_utf8_lossy(bytes).to_string(),
                };
                println!("── {origin} ──\n{text}");
            }
            bus::Answer::Error { name, message } => {
                // RFC 05 §3: an error reply always means failure. Never dress
                // one up as success.
                eprintln!("── {origin} ── {name}: {message}");
            }
        }
    }
    Ok(())
}

struct FleetAnswerRef<'a> {
    origin: &'a str,
    answer: &'a bus::Answer,
}

/// `doctor` — fan `introspect` across the fleet and diff each reply against the
/// slice this build compiled in (RFC 08 §6).
async fn cmd_doctor(args: &BusArgs) -> Result<()> {
    let session = args.session().await?;
    let roster = bus::roster(&session, args.timeout()).await?;

    let mut findings = 0usize;
    let mut answered = 0usize;

    for (name, local_toml) in zensight_keyspace::registry::REGISTRIES {
        // `@catalog` is a service origin: a verbatim `@` chunk is structurally
        // unmatchable by the `*` of a fleet selector (property D4), so it takes
        // its own key. That is the grammar working, not an exception to it.
        let key = if *name == "catalog" {
            zensight_common::catalog_rpc_key("introspect")
        } else {
            zensight_common::fleet_rpc_key(name, "introspect")
        };

        let answers = bus::fleet_get(&session, &key, None, args.timeout()).await?;
        let local = zensight_keyspace::parse_slice(local_toml)
            .map_err(|e| anyhow::anyhow!("local slice for {name} does not parse: {e}"))?;

        for answer in &answers {
            let bus::Answer::Value(bytes) = &answer.answer else {
                continue;
            };
            answered += 1;
            let served_toml = String::from_utf8_lossy(bytes);
            let served = match zensight_keyspace::parse_slice(&served_toml) {
                Ok(s) => s,
                Err(e) => {
                    println!(
                        "✗ {}/{name}: served slice does not parse: {e}",
                        answer.origin
                    );
                    findings += 1;
                    continue;
                }
            };
            let diff = zensight_keyspace::slice::diff(&served, &local);
            if diff.is_empty() {
                println!(
                    "✓ {}/{name}: in sync (registry {})",
                    answer.origin, served.version
                );
            } else {
                for f in &diff {
                    println!("✗ {}/{name}: {}", answer.origin, f.summary());
                    findings += 1;
                }
            }
        }
    }

    // The roster is what makes silence legible (RFC 05 §3.1): a producer that
    // holds an `alive` token but did not answer `introspect` is a bug, because
    // producers MUST declare their @rpc queryables *before* their token —
    // "alive ⇒ callable" (RFC 04 §5).
    let live: usize = roster.values().map(Vec::len).sum();
    println!("\n{answered} introspect repl(y|ies) from {live} live producer(s).");
    if answered < live {
        println!(
            "⚠ {} live producer(s) did not answer introspect — alive ⇒ callable (RFC 04 §5), so",
            live - answered
        );
        println!("  this is a finding, not a boot race.");
        findings += live - answered;
    }

    if findings == 0 {
        println!("no findings — the fleet agrees with this build.");
    } else {
        println!("{findings} finding(s).");
    }
    Ok(())
}
