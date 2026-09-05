//! Opt-in camera discovery (#410): browse, probe, **propose**, and stop there.
//!
//! Two probes, each named explicitly in config: an mDNS `_rtsp._tcp` browse and
//! a WS-Discovery `Probe` — the ONVIF device-discovery handshake.
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
//! **Why there is no `onvif-rs` here.** The obvious library is git-only and
//! unreleased; this workspace has zero git dependencies and `deny.toml` sets
//! `unknown-git = "deny"`. WS-Discovery itself is one SOAP datagram to a
//! multicast group and a reply to parse, so it is written out rather than
//! bought with a permanent exemption on an unreleased crate.
//!
//! What that costs is worth stating plainly: WS-Discovery returns a device's
//! **service** address, not a stream URI. Turning one into the other is an
//! ONVIF Media `GetStreamUri` call — a different protocol surface, with
//! authentication. So a WS-Discovery find is proposed with its service address
//! and the operator supplies the RTSP URL, which is what "propose" already
//! means here. A library would not have changed that; it would only have made
//! the missing half look closer.
//!
//! **Operational note, and it belongs in the operator's face rather than a
//! footnote:** both probes are multicast on a network you may not own, and
//! they are traffic an IDS can flag — the same caution `zensight-sensor-snmp`'s
//! subnet sweep carries. Keep them to networks you operate.

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

    /// Send a WS-Discovery `Probe` and collect `ProbeMatches`. Default
    /// **false**, for the same reason as `mdns`.
    #[serde(default)]
    pub ws_discovery: bool,

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
        if !self.mdns && !self.ws_discovery {
            bail!(
                "parallax.discovery is configured but enables no probe: set `mdns: true` \
                 and/or `ws_discovery: true`, or remove the block. An empty discovery \
                 block that silently does nothing is worse than no block"
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

// ── WS-Discovery (ONVIF device discovery) ───────────────────────────────────

/// The WS-Discovery multicast group and port (WS-Discovery 1.1, ONVIF Core).
const WSD_GROUP: &str = "239.255.255.250:3702";

/// The ONVIF `NetworkVideoTransmitter` device type — a camera, as opposed to
/// the NVRs, displays and printers that also answer WS-Discovery.
const WSD_PROBE: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope""#,
    r#" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing""#,
    r#" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery""#,
    r#" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">"#,
    "<e:Header>",
    "<w:MessageID>{MSGID}</w:MessageID>",
    "<w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>",
    "<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>",
    "</e:Header>",
    "<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>",
    "</e:Envelope>",
);

/// One WS-Discovery round: send a `Probe`, collect `ProbeMatches`.
///
/// **No multicast group is joined.** A `Probe` goes *to* the group from an
/// ephemeral port and every device answers **unicast** to that source port, so
/// receiving needs nothing but the socket that sent. Joining would additionally
/// subscribe this host to every other WS-Discovery conversation on the segment,
/// which a monitoring sensor has no business doing.
pub async fn probe_ws_discovery(
    probe_secs: u64,
    configured: &HashSet<String>,
) -> Result<Vec<DiscoveredStream>> {
    probe_ws_discovery_at(WSD_GROUP, probe_secs, configured).await
}

/// The body of [`probe_ws_discovery`], with the destination injected.
///
/// It exists so the socket path — send, receive, parse, propose — can be tested
/// against a responder on loopback. The parser has unit tests; without this the
/// half that actually talks to the network would have none, and "the parser is
/// correct" is not the same claim as "the probe works".
async fn probe_ws_discovery_at(
    target: &str,
    probe_secs: u64,
    configured: &HashSet<String>,
) -> Result<Vec<DiscoveredStream>> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    // TTL 1: WS-Discovery is link-local by design, and a probe that escapes the
    // segment is a probe on somebody else's network.
    socket.set_multicast_ttl_v4(1)?;

    // The MessageID must be unique per probe; a device may drop a repeat.
    let msg_id = format!("urn:uuid:{}", zensight_common::current_timestamp_millis());
    let probe = WSD_PROBE.replace("{MSGID}", &msg_id);
    socket.send_to(probe.as_bytes(), target).await?;

    let mut found: BTreeMap<String, DiscoveredStream> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(probe_secs);
    // ONVIF caps a discovery message at 32 KiB; anything larger is not a reply
    // this understands, and a bigger buffer would only make a hostile sender's
    // job easier.
    let mut buf = vec![0u8; 32 * 1024];

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok((len, from))) = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await
        else {
            break;
        };
        let Ok(body) = std::str::from_utf8(&buf[..len]) else {
            tracing::debug!(%from, "ws-discovery: reply is not UTF-8; ignored");
            continue;
        };
        let Some(matched) = parse_probe_match(body) else {
            continue;
        };

        // The device's own address is what it advertised in XAddrs, not the
        // packet's source: a device behind two interfaces answers from one and
        // serves on the other, and the operator has to reach the one it named.
        let Some(service) = matched.xaddrs.first() else {
            continue;
        };
        let address = authority_of(service).unwrap_or_else(|| from.to_string());
        if configured.contains(&address) || configured.contains(service) {
            continue;
        }

        let mut attributes: BTreeMap<String, String> = BTreeMap::new();
        attributes.insert("service".to_string(), service.clone());
        if !matched.scopes.is_empty() {
            attributes.insert("scopes".to_string(), matched.scopes.join(" "));
        }
        if !matched.types.is_empty() {
            attributes.insert("types".to_string(), matched.types.clone());
        }
        // ONVIF puts the human-facing bits in scopes as onvif://www.onvif.org/
        // name/<name> and /hardware/<model>. Lifted into their own keys because
        // a name is what an operator recognises; the raw scopes stay beside
        // them rather than being replaced by this crate's reading of them.
        if let Some(name) = scope_value(&matched.scopes, "name") {
            attributes.insert("name".to_string(), name.clone());
        }
        if let Some(hw) = scope_value(&matched.scopes, "hardware") {
            attributes.insert("hardware".to_string(), hw);
        }

        let name = scope_value(&matched.scopes, "name")
            .unwrap_or_else(|| address.split(':').next().unwrap_or(&address).to_string());
        // No URL, ever, from this probe: XAddrs is the DEVICE service, and the
        // stream URI needs an ONVIF Media GetStreamUri call. Proposing the
        // service address as an RTSP URL would be a guess that fails at connect.
        let suggested = suggest(&name, &address, None);
        found.insert(
            address.clone(),
            DiscoveredStream {
                via: "ws-discovery".to_string(),
                address,
                name: Some(name),
                url: None,
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

    Ok(found.into_values().collect())
}

/// What a `ProbeMatch` told us.
#[derive(Debug, Default, PartialEq)]
struct ProbeMatch {
    xaddrs: Vec<String>,
    scopes: Vec<String>,
    types: String,
}

/// Pull the `ProbeMatch` fields out of a SOAP envelope.
///
/// Matched on **local names**, ignoring namespace prefixes: vendors disagree
/// about whether the discovery namespace is `d:`, `wsd:`, `tds:` or unprefixed,
/// and a parser that insisted on one would silently find nothing on half the
/// cameras on the market.
///
/// Returns `None` for anything that is not a `ProbeMatches` — including the
/// `Hello`/`Bye` announcements that share the group, and any reply that carries
/// no `XAddrs`, which is a device with nowhere to point an operator.
fn parse_probe_match(xml: &str) -> Option<ProbeMatch> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut current: Option<&'static str> = None;
    let mut out = ProbeMatch::default();
    let mut saw_probe_matches = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_string();
                current = match local.as_str() {
                    "XAddrs" => Some("xaddrs"),
                    "Scopes" => Some("scopes"),
                    "Types" => Some("types"),
                    other => {
                        if other == "ProbeMatches" {
                            saw_probe_matches = true;
                        }
                        None
                    }
                };
            }
            Ok(Event::Text(t)) => {
                let Some(field) = current else { continue };
                let text = t.xml10_content().trim().to_string();
                if text.is_empty() {
                    continue;
                }
                match field {
                    // Both XAddrs and Scopes are space-separated lists.
                    "xaddrs" => out
                        .xaddrs
                        .extend(text.split_whitespace().map(str::to_string)),
                    "scopes" => out
                        .scopes
                        .extend(text.split_whitespace().map(str::to_string)),
                    _ => out.types = text,
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) => break,
            // A malformed datagram from the network is ignored, never fatal:
            // this loop is fed by anything that can reach a UDP port.
            Err(_) => return None,
            _ => {}
        }
    }

    (saw_probe_matches && !out.xaddrs.is_empty()).then_some(out)
}

/// `http://10.0.0.7:8000/onvif/device_service` -> `10.0.0.7:8000`.
fn authority_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    (!authority.is_empty()).then(|| authority.to_string())
}

/// The tail of an ONVIF scope: `onvif://www.onvif.org/name/Front%20Door` with
/// `"name"` gives `Front Door`.
///
/// Percent-decoding is done here rather than left to a caller because a scope
/// is where the human-readable name lives and `%20` in a proposal is the kind
/// of detail that makes an operator distrust the whole document.
fn scope_value(scopes: &[String], key: &str) -> Option<String> {
    let needle = format!("/{key}/");
    let raw = scopes.iter().find_map(|s| s.split_once(&needle))?.1;
    let raw = raw.split('/').next().unwrap_or(raw);
    (!raw.is_empty()).then(|| percent_decode(raw))
}

/// Minimal percent-decoding for scope values. Anything that is not a valid
/// `%XX` pair is left exactly as it arrived rather than guessed at.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
            ws_discovery: false,
            browse_secs: 10,
            interval_secs: 3600,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_round_that_cannot_finish_before_the_next_is_refused() {
        let cfg = DiscoveryConfig {
            mdns: true,
            ws_discovery: false,
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
            ws_discovery: false,
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

    // ── WS-Discovery parsing (#410) ──────────────────────────────────────

    /// A real-shaped ProbeMatches, with the `d:`/`wsa:` prefixes an ONVIF
    /// camera actually sends.
    const PROBE_MATCHES: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
 xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
 xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
 xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<SOAP-ENV:Header><wsa:MessageID>urn:uuid:1</wsa:MessageID></SOAP-ENV:Header>
<SOAP-ENV:Body><d:ProbeMatches><d:ProbeMatch>
<wsa:EndpointReference><wsa:Address>urn:uuid:cam-1</wsa:Address></wsa:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Front%20Door onvif://www.onvif.org/hardware/ACME-C1</d:Scopes>
<d:XAddrs>http://10.0.0.7:8000/onvif/device_service</d:XAddrs>
<d:MetadataVersion>1</d:MetadataVersion>
</d:ProbeMatch></d:ProbeMatches></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;

    #[test]
    fn a_probe_match_yields_its_addresses_scopes_and_types() {
        let m = parse_probe_match(PROBE_MATCHES).expect("a ProbeMatches parses");
        assert_eq!(m.xaddrs, vec!["http://10.0.0.7:8000/onvif/device_service"]);
        assert_eq!(m.types, "dn:NetworkVideoTransmitter");
        assert_eq!(m.scopes.len(), 3);
    }

    #[test]
    fn namespace_prefixes_are_ignored() {
        // Vendors disagree about whether the discovery namespace is d:, wsd:,
        // tds: or unprefixed. A parser that insisted on one would silently find
        // nothing on half the cameras on the market — silently, because "no
        // reply" and "a reply I could not read" would look identical.
        let unprefixed = PROBE_MATCHES
            .replace("d:ProbeMatch", "ProbeMatch")
            .replace("d:XAddrs", "XAddrs")
            .replace("d:Scopes", "Scopes")
            .replace("d:Types", "Types");
        let m = parse_probe_match(&unprefixed).expect("prefixes must not matter");
        assert_eq!(m.xaddrs, vec!["http://10.0.0.7:8000/onvif/device_service"]);

        let odd = PROBE_MATCHES.replace("d:", "wsd:");
        assert!(parse_probe_match(&odd).is_some());
    }

    #[test]
    fn a_hello_is_not_a_probe_match() {
        // Hello/Bye announcements share the multicast group and carry XAddrs
        // too. Treating one as a probe reply would add a device the probe never
        // asked about, at whatever moment it happened to boot.
        let hello = PROBE_MATCHES
            .replace("d:ProbeMatches", "d:Hello")
            .replace("d:ProbeMatch>", "d:HelloBody>");
        assert!(parse_probe_match(&hello).is_none());
    }

    #[test]
    fn a_reply_with_no_xaddrs_is_dropped() {
        // A device with no service address is a device with nowhere to point an
        // operator, so it is not a proposal.
        let no_addr = PROBE_MATCHES.replace(
            "<d:XAddrs>http://10.0.0.7:8000/onvif/device_service</d:XAddrs>",
            "",
        );
        assert!(parse_probe_match(&no_addr).is_none());
    }

    #[test]
    fn garbage_from_the_network_is_ignored_not_fatal() {
        // This parser is fed by anything that can reach a UDP port.
        assert!(parse_probe_match("").is_none());
        assert!(parse_probe_match("not xml at all").is_none());
        assert!(parse_probe_match("<a><b></a>").is_none());
        assert!(parse_probe_match("<d:ProbeMatches><d:XAddrs>").is_none());
    }

    #[test]
    fn the_authority_is_taken_from_the_advertised_service_url() {
        assert_eq!(
            authority_of("http://10.0.0.7:8000/onvif/device_service").as_deref(),
            Some("10.0.0.7:8000")
        );
        assert_eq!(
            authority_of("https://cam.local/x").as_deref(),
            Some("cam.local")
        );
        assert_eq!(authority_of("nonsense"), None);
    }

    #[test]
    fn onvif_scope_values_are_percent_decoded() {
        let m = parse_probe_match(PROBE_MATCHES).unwrap();
        assert_eq!(
            scope_value(&m.scopes, "name").as_deref(),
            Some("Front Door")
        );
        assert_eq!(
            scope_value(&m.scopes, "hardware").as_deref(),
            Some("ACME-C1")
        );
        assert_eq!(scope_value(&m.scopes, "serial"), None);
    }

    #[test]
    fn a_malformed_percent_escape_is_left_alone_rather_than_guessed() {
        assert_eq!(percent_decode("Front%20Door"), "Front Door");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[tokio::test]
    async fn the_probe_talks_to_a_responder_end_to_end() {
        // The parser has unit tests; this covers the other half — send,
        // receive, parse, propose — against a responder on loopback, so the
        // socket path is not shipped on the strength of the parser's tests.
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = responder.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            if let Ok((len, from)) = responder.recv_from(&mut buf).await {
                // It must be a Probe for the ONVIF device type, not any
                // datagram: a probe that asks the wrong question would find
                // cameras by accident and printers on purpose.
                let sent = String::from_utf8_lossy(&buf[..len]).to_string();
                assert!(sent.contains("NetworkVideoTransmitter"), "{sent}");
                assert!(sent.contains("discovery/Probe"), "{sent}");
                let _ = responder.send_to(PROBE_MATCHES.as_bytes(), from).await;
            }
        });

        let found = probe_ws_discovery_at(&addr, 3, &HashSet::new())
            .await
            .expect("the probe runs");
        assert_eq!(found.len(), 1, "{found:?}");
        let cam = &found[0];
        assert_eq!(cam.via, "ws-discovery");
        // The ADVERTISED address, not the packet's source: a device behind two
        // interfaces answers from one and serves on the other.
        assert_eq!(cam.address, "10.0.0.7:8000");
        assert_eq!(cam.name.as_deref(), Some("Front Door"));
        assert_eq!(cam.url, None, "WS-Discovery never yields a stream URL");
        assert!(cam.suggested.contains("<path>"), "{}", cam.suggested);
        assert_eq!(
            cam.attributes.get("hardware").map(String::as_str),
            Some("ACME-C1")
        );
        assert_eq!(
            cam.attributes.get("service").map(String::as_str),
            Some("http://10.0.0.7:8000/onvif/device_service")
        );
    }

    #[tokio::test]
    async fn an_already_configured_device_is_not_re_proposed() {
        // A proposal the operator has already accepted is noise, and noise in
        // this document trains them to stop reading it.
        let responder = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = responder.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            if let Ok((_, from)) = responder.recv_from(&mut buf).await {
                let _ = responder.send_to(PROBE_MATCHES.as_bytes(), from).await;
            }
        });
        let configured: HashSet<String> = ["10.0.0.7:8000".to_string()].into();
        let found = probe_ws_discovery_at(&addr, 3, &configured).await.unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[tokio::test]
    async fn a_silent_network_ends_the_round_on_schedule() {
        // Nothing answers: the round must end at its deadline rather than
        // holding the task open, and report nothing rather than failing.
        let sink = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sink.local_addr().unwrap().to_string();
        let start = std::time::Instant::now();
        let found = probe_ws_discovery_at(&addr, 1, &HashSet::new())
            .await
            .unwrap();
        assert!(found.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_block_enabling_only_ws_discovery_is_valid() {
        let cfg = DiscoveryConfig {
            mdns: false,
            ws_discovery: true,
            browse_secs: 10,
            interval_secs: 3600,
        };
        assert!(cfg.validate().is_ok());
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
