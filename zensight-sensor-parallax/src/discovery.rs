//! Opt-in camera discovery (#410): browse, **propose**, and stop there.
//!
//! Never runs without an explicit `parallax.discovery` block — the block's
//! presence is the opt-in, the same shape the SNMP subnet sweep (#541) uses.
//! Responders that are not already configured streams are published as a
//! [`StreamDiscoveryReport`] on `state/parallax/discovery`, each with a
//! copy-pasteable JSON5 `rtsp[]` snippet.
//!
//! **Nothing here is ever opened.** A discovered camera does not enter the
//! catalogue, gets no liveliness token, and is never captured, encoded or
//! published. `auto_add` is deliberately not implemented — proposal is the only
//! mode, exactly as for SNMP.
//!
//! That matters more here than it looks. A camera is a device with a view of a
//! room, and a monitoring system that starts pulling video off hardware nobody
//! configured has done something categorically different from noticing it
//! exists. Discovery answers *"what is out there, so I can write a config"*.
//!
//! **Operational note, and it belongs in the operator's face rather than a
//! footnote:** mDNS is multicast on a network you may not own, and it is
//! traffic an IDS can flag — the same caution `zensight-sensor-snmp`'s subnet
//! sweep carries. Keep it to networks you operate.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use zensight_common::{DiscoveredStream, StreamDiscoveryReport};

/// The `_rtsp._tcp` service type, as RFC 6763 spells it.
const RTSP_SERVICE: &str = "_rtsp._tcp.local.";

/// Hard cap on responders kept per round.
///
/// A discovery document is LWW state read by a GUI, not a log: a misconfigured
/// or hostile network answering ten thousand times must not turn one publish
/// into something a frontend has to render. Sized like the SNMP sweep's address
/// cap, and for the same reason — a bound the operator did not have to think of.
pub const MAX_DISCOVERED: usize = 256;

/// Discovery configuration (#410). The block's presence is the opt-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// Browse `_rtsp._tcp` over mDNS. Default **false**: even inside an opt-in
    /// block, each probe is named explicitly, so enabling discovery never turns
    /// on a protocol the operator did not ask for.
    #[serde(default)]
    pub mdns: bool,

    /// How long each browse round listens, in seconds (default 10).
    #[serde(default = "default_browse_secs")]
    pub browse_secs: u64,

    /// Seconds between rounds (default 3600).
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

fn default_browse_secs() -> u64 {
    10
}
fn default_interval_secs() -> u64 {
    3600
}

impl DiscoveryConfig {
    /// Reject at startup what would otherwise be a surprise at runtime.
    pub fn validate(&self) -> Result<()> {
        if !self.mdns {
            bail!(
                "parallax.discovery is configured but enables no probe: set `mdns: true`, \
                 or remove the block. An empty discovery block that silently does nothing \
                 is worse than no block"
            );
        }
        if self.browse_secs == 0 {
            bail!("parallax.discovery.browse_secs must be > 0");
        }
        if self.interval_secs < self.browse_secs {
            bail!(
                "parallax.discovery.interval_secs ({}) is below browse_secs ({}): the next \
                 round would start before this one finished",
                self.interval_secs,
                self.browse_secs
            );
        }
        Ok(())
    }
}

/// One browse round over mDNS `_rtsp._tcp`.
///
/// `configured` is every address and URL the catalogue already carries;
/// anything matching is not re-proposed, because a proposal an operator has
/// already accepted is noise that trains them to ignore the document.
pub async fn browse_mdns(
    browse_secs: u64,
    configured: &HashSet<String>,
) -> Result<Vec<DiscoveredStream>> {
    // mdns-sd runs its own thread and hands events over a channel, so the
    // daemon is created and dropped inside this call: a browse that is only
    // performed once an hour has no reason to hold a socket for the other
    // fifty-nine minutes.
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let receiver = daemon.browse(RTSP_SERVICE)?;

    let mut found: BTreeMap<String, DiscoveredStream> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(browse_secs);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // `recv_async` is flume's; the timeout is ours, so a silent network
        // ends the round on schedule instead of holding the task open.
        let event = match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Err(_) => break,
            Ok(Err(_)) => break,
            Ok(Ok(event)) => event,
        };

        let mdns_sd::ServiceEvent::ServiceResolved(service) = event else {
            // Every other variant is progress reporting — SearchStarted,
            // ServiceFound (a name, no address yet), removals, SearchStopped.
            // Only a resolved service has somewhere to point an operator.
            continue;
        };

        // `addresses` is a HashSet, so `.iter().next()` is a DIFFERENT address
        // from round to round on a multi-homed camera. This document is LWW
        // state published every round: an address that changes for no reason
        // rewrites it for no reason, which is the churn the compiler and the
        // historian both go out of their way to avoid. So pick
        // deterministically — IPv4 first, because an operator's RTSP URL almost
        // always is, then lowest address.
        let mut addrs: Vec<std::net::IpAddr> =
            service.addresses.iter().map(|a| a.to_ip_addr()).collect();
        addrs.sort_by_key(|a| (a.is_ipv6(), a.to_string()));
        let Some(addr) = addrs.first() else {
            continue;
        };
        let address = format!("{addr}:{}", service.port);
        if configured.contains(&address) {
            continue;
        }

        let attributes: BTreeMap<String, String> = service
            .txt_properties
            .iter()
            .map(|p| (p.key().to_string(), p.val_str().to_string()))
            .collect();

        // RFC 6763 §6.5 gives `path` as the conventional TXT key for a URL
        // path. Most cameras do not publish one, which is why `url` is
        // optional and the snippet below carries a visible placeholder rather
        // than a guess that would fail at connect time.
        let url = attributes.get("path").map(|p| {
            format!(
                "rtsp://{address}{}",
                if p.starts_with('/') {
                    p.clone()
                } else {
                    format!("/{p}")
                }
            )
        });

        let name = instance_name(&service.fullname);
        if url.as_ref().is_some_and(|u| configured.contains(u)) {
            continue;
        }

        let suggested = suggest(&name, &address, url.as_deref());
        found.insert(
            address.clone(),
            DiscoveredStream {
                via: "mdns".to_string(),
                address,
                name: Some(name),
                url,
                attributes,
                suggested,
            },
        );
        if found.len() >= MAX_DISCOVERED {
            tracing::warn!(
                cap = MAX_DISCOVERED,
                "discovery: responder cap reached; the round is truncated"
            );
            break;
        }
    }

    // Dropping the daemon is not enough on its own — shut it down explicitly so
    // the socket is released now rather than whenever the thread notices.
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

/// Assemble a report from a round's finds.
pub fn report(methods: Vec<String>, discovered: Vec<DiscoveredStream>) -> StreamDiscoveryReport {
    StreamDiscoveryReport {
        timestamp: zensight_common::current_timestamp_millis(),
        methods,
        discovered,
    }
}

/// `cam-1._rtsp._tcp.local.` → `cam-1`.
fn instance_name(fullname: &str) -> String {
    fullname
        .split_once("._rtsp._tcp")
        .map(|(instance, _)| instance)
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_string()
}

/// The copy-pasteable `parallax.rtsp[]` entry.
///
/// With no URL the placeholder is deliberately obvious — `<path>` will not
/// connect, and an operator pasting it unedited gets an error naming the stream
/// rather than a silently wrong URL that looks configured.
fn suggest(name: &str, address: &str, url: Option<&str>) -> String {
    let stream = stream_name(name, address);
    match url {
        Some(url) => format!("{{ name: \"{stream}\", url: \"{url}\" }}"),
        None => format!("{{ name: \"{stream}\", url: \"rtsp://{address}/<path>\" }}"),
    }
}

/// A stream name a config would accept: the advertised instance name reduced to
/// the catalogue's charset, or the address when nothing usable survives.
fn stream_name(name: &str, address: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        address.replace([':', '.'], "-")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_discovery_block_is_refused_at_startup() {
        // The failure this prevents is quiet: a block that enables no probe
        // parses, runs, publishes an empty report forever, and reads as "there
        // are no cameras" rather than "you did not turn anything on".
        let cfg = DiscoveryConfig {
            mdns: false,
            browse_secs: 10,
            interval_secs: 3600,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_round_that_cannot_finish_before_the_next_is_refused() {
        let cfg = DiscoveryConfig {
            mdns: true,
            browse_secs: 60,
            interval_secs: 30,
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("interval_secs"), "{err}");
    }

    #[test]
    fn a_valid_block_passes() {
        let cfg = DiscoveryConfig {
            mdns: true,
            browse_secs: 10,
            interval_secs: 3600,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn instance_names_lose_the_service_suffix() {
        assert_eq!(instance_name("front-door._rtsp._tcp.local."), "front-door");
        assert_eq!(instance_name("odd name"), "odd name");
    }

    #[test]
    fn a_suggestion_without_a_url_carries_a_visible_placeholder() {
        // An operator must be able to see that something is missing. A guessed
        // path would look configured and fail at connect time, which is the
        // worse of the two failures.
        let s = suggest("Front Door", "10.0.0.7:554", None);
        assert!(s.contains("<path>"), "{s}");
        assert!(s.contains("front-door"), "{s}");
    }

    #[test]
    fn a_suggestion_with_a_url_uses_it() {
        let s = suggest("cam1", "10.0.0.7:554", Some("rtsp://10.0.0.7:554/stream1"));
        assert_eq!(
            s,
            "{ name: \"cam1\", url: \"rtsp://10.0.0.7:554/stream1\" }"
        );
        assert!(!s.contains("<path>"));
    }

    #[test]
    fn stream_names_survive_hostile_instance_names() {
        // mDNS instance names are free-form UTF-8 with spaces and punctuation;
        // a stream name is one key chunk. A name that cannot be a chunk must
        // become the address rather than a key the sensor would then refuse.
        assert_eq!(
            stream_name("Front Door Cam", "10.0.0.7:554"),
            "front-door-cam"
        );
        assert_eq!(stream_name("!!!", "10.0.0.7:554"), "10-0-0-7-554");
        assert_eq!(stream_name("", "10.0.0.7:554"), "10-0-0-7-554");
    }
}
