//! The one-shot discovery mode (#825 item 4).
//!
//! *"The gap between 'supported' and 'usable' for SNMP is always the config."*
//! `--discover <cidr>` walks a subnet, identifies what answers by `sysName`,
//! `sysObjectID` and `sysDescr`, and **prints a proposed config to stdout**.
//!
//! It never applies one, and it never touches the bus: the runner, the Zenoh
//! session and the publishers are not constructed at all on this path. That is
//! deliberate and structural rather than a promise — an operator running a
//! subnet sweep from a laptop should not thereby join a fleet.
//!
//! It is distinct from the `snmp.discovery` config block, which is a
//! *continuous* sweep inside the running sensor publishing a `DiscoveryReport`
//! on the bus. Both exist because they answer different questions: "what is out
//! there right now, so I can write a config" and "what appeared on my network
//! since I last looked".

use anyhow::{Context, Result};
use clap::Parser;

use crate::config::{SnmpSensorConfig, SnmpVersion};
use crate::discovery::{Discovery, DiscoveryConfig};

/// The SNMP sensor's CLI: the shared sensor arguments plus this sensor's own
/// one-shot mode.
#[derive(Parser, Debug, Clone)]
#[command(about = "ZenSight SNMP sensor")]
pub struct SnmpArgs {
    #[command(flatten)]
    pub common: zensight_sensor_core::SensorArgs,

    /// Sweep a subnet, identify what answers, and print a proposed config —
    /// then exit. Never applies anything, never connects to the bus.
    #[arg(long, value_name = "CIDR")]
    pub discover: Option<String>,

    /// Credential sets from the config to try, in order. Default: every set
    /// the config defines; failing that, the classic `public` community.
    #[arg(long, value_name = "NAME")]
    pub discover_credentials: Vec<String>,

    /// UDP port to probe.
    #[arg(long, default_value_t = 161)]
    pub discover_port: u16,

    /// Per-address probe timeout, seconds.
    #[arg(long, default_value_t = 1)]
    pub discover_timeout: u64,

    /// Concurrent probes.
    #[arg(long, default_value_t = 32)]
    pub discover_concurrency: usize,
}

impl SnmpArgs {
    /// Parse, defaulting `--config` the way `SensorArgs::parse_with_default`
    /// does for every other sensor.
    pub fn parse_with_default(default_config: &'static str) -> Self {
        let matches = <Self as clap::CommandFactory>::command()
            .mut_arg("config", |arg| arg.default_value(default_config))
            .get_matches();
        <Self as clap::FromArgMatches>::from_arg_matches(&matches)
            .expect("failed to parse arguments")
    }
}

/// Run the sweep and print the proposal. Returns the number of responders.
///
/// The config is read only for its credential sets and its device list: the
/// former so a sweep can authenticate, the latter so already-configured
/// addresses are never proposed a second time.
pub async fn discover(args: &SnmpArgs, cidr: &str) -> Result<usize> {
    let config = SnmpSensorConfig::load(&args.common.config)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| {
            format!(
                "discovery reads {} for its credential sets and its existing \
                 device list",
                args.common.config.display()
            )
        })?;

    let wanted = &args.discover_credentials;
    let credentials: Vec<(String, crate::config::CredentialSet)> = config
        .snmp
        .credentials
        .iter()
        .filter(|(name, _)| wanted.is_empty() || wanted.contains(name))
        .map(|(name, set)| (name.clone(), set.clone()))
        .collect();
    if !wanted.is_empty() && credentials.len() != wanted.len() {
        let known: Vec<&String> = config.snmp.credentials.keys().collect();
        anyhow::bail!("unknown credential set; the config defines {known:?}");
    }

    // Never re-propose something already configured. An operator running this
    // against a subnet they already monitor should get the NEW devices, not a
    // fresh copy of their own config.
    let known: std::collections::HashSet<String> = config
        .snmp
        .devices
        .iter()
        .filter_map(|d| {
            d.address
                .rsplit_once(':')
                .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
        })
        .collect();
    let already = known.len();

    let discovery = Discovery::new(
        DiscoveryConfig {
            subnets: vec![cidr.to_string()],
            credentials: Vec::new(),
            port: args.discover_port,
            max_concurrency: args.discover_concurrency,
            probe_timeout_secs: args.discover_timeout,
            // Irrelevant on a one-shot sweep: there is no next round.
            interval_secs: 0,
        },
        credentials,
        std::sync::Arc::new(std::sync::RwLock::new(known)),
    );

    let addresses = discovery.addresses()?;
    eprintln!(
        "sweeping {cidr} — {} address(es) on udp/{}, {already} already configured and \
         skipped, {} concurrent, {}s per probe",
        addresses.len(),
        args.discover_port,
        args.discover_concurrency,
        args.discover_timeout,
    );
    let report = discovery.sweep(&addresses).await;
    // Progress and diagnostics on stderr, the proposal on stdout — so
    // `--discover 10.0.0.0/24 > devices.json5` yields a file that is only the
    // proposal.
    eprintln!(
        "scanned {} address(es), {} responded",
        report.scanned,
        report.discovered.len()
    );

    print!("{}", render(&report.discovered, cidr));
    Ok(report.discovered.len())
}

/// Render the proposal: a `devices: [...]` block, commented with what each
/// device said about itself.
pub fn render(devices: &[zensight_common::discovery::DiscoveredDevice], cidr: &str) -> String {
    if devices.is_empty() {
        return format!(
            "// No SNMP responder found in {cidr}.\n\
             //\n\
             // That is a finding, not necessarily an error: nothing answered, the\n\
             // credentials did not match, or a firewall dropped udp/161. Silence from\n\
             // an SNMP agent is indistinguishable from silence from a filtered port.\n"
        );
    }
    let mut out = String::new();
    out.push_str(&format!(
        "// Proposed by `zensight-sensor-snmp --discover {cidr}` — NOTHING WAS APPLIED.\n\
         // Review every line before pasting it into `snmp.devices`. In particular:\n\
         //   * `name` is taken from sysName, or from the address when there is none.\n\
         //     It becomes the device slug in every key, so make it one you want.\n\
         //   * a device that answered a v1/v2c community answered a CLEARTEXT\n\
         //     credential, and `snmp.allow_insecure_versions` must be set for the\n\
         //     sensor to accept it at all (#825 item 1).\n\
         //   * give each device a `max_pdus_per_sec` if it is older or smaller than\n\
         //     the machine polling it (#825 item 2).\n\
         devices: [\n"
    ));
    for d in devices {
        if let Some(n) = &d.sys_name {
            out.push_str(&format!("  // sysName:     {n}\n"));
        }
        if let Some(o) = &d.sys_object_id {
            out.push_str(&format!("  // sysObjectID: {o}\n"));
        }
        if let Some(s) = &d.sys_descr {
            // One line, bounded: a sysDescr can be a whole paragraph of
            // firmware banner.
            let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            let flat: String = flat.chars().take(160).collect();
            out.push_str(&format!("  // sysDescr:    {flat}\n"));
        }
        if !d.matched_profiles.is_empty() {
            out.push_str(&format!(
                "  // profiles:    {}\n",
                d.matched_profiles.join(", ")
            ));
        }
        out.push_str(&format!("  {},\n", d.suggested));
    }
    out.push_str("]\n");
    out
}

/// Whether a version needs `allow_insecure_versions` — used by the proposal's
/// header note.
pub fn is_cleartext(v: SnmpVersion) -> bool {
    matches!(v, SnmpVersion::V1 | SnmpVersion::V2c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::discovery::DiscoveredDevice;

    fn device(addr: &str, name: Option<&str>) -> DiscoveredDevice {
        DiscoveredDevice {
            address: addr.to_string(),
            credentials: Some("readonly".into()),
            sys_object_id: Some("1.3.6.1.4.1.9.1.1".into()),
            sys_name: name.map(str::to_string),
            sys_descr: Some("Cisco IOS Software, C2960 Software\n  Version 15.0".into()),
            matched_profiles: vec!["cisco-switch".into()],
            suggested: format!("{{ name: \"sw1\", address: \"{addr}\" }}"),
        }
    }

    /// The proposal must say, in the output itself, that nothing was applied.
    /// Someone will paste this into a terminal at 2 a.m.
    #[test]
    fn the_proposal_says_it_applied_nothing() {
        let out = render(&[device("10.0.0.1:161", Some("sw1"))], "10.0.0.0/24");
        assert!(out.contains("NOTHING WAS APPLIED"), "{out}");
        assert!(out.starts_with("// Proposed by"), "{out}");
    }

    #[test]
    fn each_device_is_annotated_with_what_it_said_about_itself() {
        let out = render(&[device("10.0.0.1:161", Some("sw1"))], "10.0.0.0/24");
        assert!(out.contains("// sysName:     sw1"), "{out}");
        assert!(out.contains("// sysObjectID: 1.3.6.1.4.1.9.1.1"), "{out}");
        assert!(out.contains("// profiles:    cisco-switch"), "{out}");
        assert!(
            out.contains("{ name: \"sw1\", address: \"10.0.0.1:161\" }"),
            "{out}"
        );
    }

    /// A sysDescr is often a multi-line firmware banner; pasting it raw would
    /// produce a config file that does not parse.
    #[test]
    fn a_multiline_sysdescr_becomes_one_bounded_comment_line() {
        let out = render(&[device("10.0.0.1:161", Some("sw1"))], "10.0.0.0/24");
        let descr: Vec<&str> = out.lines().filter(|l| l.contains("sysDescr")).collect();
        assert_eq!(descr.len(), 1, "{out}");
        assert!(descr[0].contains("Cisco IOS Software, C2960 Software Version 15.0"));
        for line in out.lines() {
            assert!(line.len() <= 200, "a line ran away: {line}");
        }
    }

    /// Nothing answering is a finding worth a sentence, not an empty file that
    /// reads as "there is nothing there".
    #[test]
    fn an_empty_sweep_explains_itself() {
        let out = render(&[], "10.0.0.0/24");
        assert!(
            out.contains("No SNMP responder found in 10.0.0.0/24"),
            "{out}"
        );
        assert!(
            out.contains("indistinguishable from silence from a filtered port"),
            "{out}"
        );
    }

    #[test]
    fn the_proposal_warns_about_cleartext_credentials_and_budgets() {
        let out = render(&[device("10.0.0.1:161", None)], "10.0.0.0/24");
        assert!(out.contains("allow_insecure_versions"), "{out}");
        assert!(out.contains("max_pdus_per_sec"), "{out}");
        assert!(is_cleartext(SnmpVersion::V2c));
        assert!(!is_cleartext(SnmpVersion::V3));
    }

    /// The CLI must still behave like every other sensor's when no discovery
    /// flag is present.
    #[test]
    fn the_common_arguments_are_still_there() {
        use clap::Parser;
        let a = SnmpArgs::try_parse_from(["snmp", "--config", "/tmp/x.json5"]).unwrap();
        assert_eq!(a.common.config.to_str(), Some("/tmp/x.json5"));
        assert!(a.discover.is_none());

        let d = SnmpArgs::try_parse_from([
            "snmp",
            "--config",
            "/tmp/x.json5",
            "--discover",
            "10.0.0.0/24",
            "--discover-port",
            "1161",
        ])
        .unwrap();
        assert_eq!(d.discover.as_deref(), Some("10.0.0.0/24"));
        assert_eq!(d.discover_port, 1161);
    }
}
