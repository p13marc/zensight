//! Gated PDU outlet control (#956, SYS-SUP-003).
//!
//! ZenSight could not power-cycle anything. The only write surface in the
//! platform was the `systemd` sensor's gated action set (#283), and this is
//! that pattern applied to an outlet — with a stricter gate, because **a
//! monitor that can cut power is a different threat model**. That is the
//! sentence `zensight-sensor-pve` and `zensight-sensor-bmc` use to justify
//! having no action surface at all, and it is why this is its own issue with
//! its own decision rather than a corner of #955.
//!
//! # The gate, in four independent parts
//!
//! Every one of them must pass, and every refusal **names the switch that
//! refused it** (#866) so an operator learns which from the answer:
//!
//! 1. `actions.enabled` — the master switch, default **off**, so every
//!    existing deployment is unaffected by this code existing.
//! 2. `actions.allow_outlets` — `<device>/<outlet>` globs, default **empty**,
//!    which rejects everything even with the switch on. There is deliberately
//!    no `allow_all`: a wildcard an operator typed is a decision, a wildcard a
//!    default provided is an accident.
//! 3. A **separate write credential**, refused at startup if missing. A read
//!    community that can reach a SET is a control credential nobody decided
//!    to grant.
//! 4. The device must be pinned to a PDU profile whose control OIDs this build
//!    has **verified against the vendor MIB**. Guessing a control OID is worse
//!    than guessing a read one: the read publishes a wrong number, the write
//!    does something to a machine.
//!
//! # What this is honest about
//!
//! There is **no polkit here**. A PDU speaks SNMP; there is no local policy
//! engine between us and it, so the allowlist is the only gate. And the bus
//! caller is anonymous: #957 makes the attempt *auditable*, not
//! *attributable*. Until a caller identity exists (Zenoh mTLS certificate CN
//! plus Zenoh's ACL — a scope question named in epic #952), the true sentence
//! is **"anyone who can reach the bus and whose target is on the allowlist"**.
//!
//! # The pure part
//!
//! [`capability`], [`gate`] and [`supported_profile`] take configuration and
//! return an answer. No bus, no SNMP, no clock — so the whole policy is
//! testable, and [`advertised_capability_agrees_with_the_gate`] can check that
//! what the frontend previews is what the sensor will actually do.
//!
//! [`advertised_capability_agrees_with_the_gate`]: #

use std::collections::VecDeque;
use std::sync::Arc;

use async_snmp::{Oid, Value};
use tokio::sync::Mutex;
use zensight_common::audit::Refusal;
use zensight_common::command::{command_key, nested_query_key, status_key};
use zensight_common::outlet::{OutletAction, OutletCapability, OutletStatus, OutletVerb};
use zensight_common::served::WriteQuery;

use crate::config::{ActionsConfig, DeviceConfig, SnmpConfig};

/// The `@rpc` topic this control surface lives under.
pub const ACTION_TOPIC: &str = "action";
/// The audit-timeline ring's procedure name (plural — `action` is the
/// singular "most recent").
pub const ACTIONS_TOPIC: &str = "actions";

/// PDU profiles whose **control** OIDs this build has verified against the
/// vendor MIB.
///
/// `pdu-eaton` and `pdu-raritan` are read-only for now, and that is the point
/// of the list: their outlet *status* columns were verified for #955, their
/// *control* columns were not. Guessing a control OID is categorically worse
/// than guessing a read one — a wrong read publishes a wrong number, a wrong
/// write does something to a machine.
pub const CONTROLLABLE_PROFILES: &[&str] = &["pdu-apc"];

/// `rPDU2OutletSwitchedControlCommand`, verified against PowerNet-MIB v4.5.8
/// (`rPDU2` = hardware 26, `rPDU2Outlet` = 9, `Switched` = 2, `ControlTable`
/// = 4, entry 1, column 5). `immediateReboot(3)` is the cycle.
const APC_CONTROL_COMMAND: &str = "1.3.6.1.4.1.318.1.1.26.9.2.4.1.5";
const APC_IMMEDIATE_REBOOT: i32 = 3;
/// `rPDU2OutletSwitchedStatusState` — read before and after, off(1) on(2).
const APC_STATUS_STATE: &str = "1.3.6.1.4.1.318.1.1.26.9.2.3.1.5";
/// `rPDU2OutletSwitchedConfigRebootDuration` — the PDU's OWN off-time, 5–60 s.
/// Reported, never chosen by us: the delay belongs to the device.
const APC_REBOOT_DURATION: &str = "1.3.6.1.4.1.318.1.1.26.9.2.1.1.7";

/// The gate state this configuration advertises. Pure.
pub fn capability(cfg: &ActionsConfig) -> OutletCapability {
    if !cfg.enabled {
        // The reason travels with the answer (#866). "Disabled" is already an
        // answer rather than a silence; without the why, a greyed-out button
        // is still indistinguishable from a broken one and the operator has no
        // way to learn which file to edit.
        return OutletCapability::disabled_because(
            "actions.enabled is false in this sensor's config (configs/snmp.json5) — the \
             sensor is deliberately read-only. Set actions.enabled, add an \
             actions.allow_outlets allowlist and a write credential set, to permit gated \
             outlet control.",
        );
    }
    // Enabled with an empty allowlist accepts nothing: a distinct fact from
    // "switched off", and one an operator would otherwise diagnose by trying.
    let reason = cfg.allow_outlets.is_empty().then(|| {
        "actions.enabled is true but actions.allow_outlets is empty, so every outlet is \
         refused. Add <device>/<outlet> globs to permit control."
            .to_string()
    });
    OutletCapability {
        enabled: true,
        allow_outlets: cfg.allow_outlets.clone(),
        verbs: OutletVerb::all(),
        reason,
    }
}

/// Whether `device` is pinned to a profile this build can control.
pub fn supported_profile(device: &DeviceConfig) -> bool {
    device
        .profile
        .as_deref()
        .is_some_and(|p| CONTROLLABLE_PROFILES.contains(&p))
}

/// Which gate rejected a request, if any. Pure, so the whole table is testable
/// without a bus or a PDU.
///
/// The [`Refusal`] carries the switch as a **field** (#957), not only inside
/// the sentence: the audit trail filters on it, and #866's "name the switch"
/// contract should not be enforced by reading English.
pub fn gate(
    cfg: &ActionsConfig,
    devices: &[DeviceConfig],
    cmd: &OutletAction,
) -> Result<(), Refusal> {
    if !cfg.enabled {
        return Err(Refusal::new(
            "actions.enabled",
            "outlet control disabled (actions.enabled = false)",
        ));
    }
    if cfg.credentials.is_none() {
        // Belt and braces: startup already refuses this, so reaching it means
        // the config was built in code. It still must not fall through to a
        // SET on the read credential.
        return Err(Refusal::new(
            "actions.credentials",
            "actions.credentials names no write credential set; an outlet cycle is an SNMP \
             SET and must not ride the read credential",
        ));
    }
    if cmd.outlet.trim().is_empty() {
        return Err(Refusal::new(
            "actions.allow_outlets",
            "empty outlet — actions.allow_outlets matches a <device>/<outlet> target, and \
             there is no outlet here to match",
        ));
    }
    let Some(device) = devices.iter().find(|d| d.name == cmd.device) else {
        return Err(Refusal::new(
            "snmp.devices",
            format!(
                "device {:?} is not in snmp.devices on this sensor — an outlet cycle can \
                 only target a PDU this sensor already polls",
                cmd.device
            ),
        ));
    };
    if !supported_profile(device) {
        return Err(Refusal::new(
            "devices[].profile",
            format!(
                "device {:?} has no devices[].profile this build can control (supported: \
                 {}). Another vendor's control OIDs have not been verified against its MIB, \
                 and guessing one would send a write to a machine",
                cmd.device,
                CONTROLLABLE_PROFILES.join(", ")
            ),
        ));
    }
    if !zensight_common::action::allows(&cfg.allow_outlets, &cmd.target()) {
        return Err(Refusal::new(
            "actions.allow_outlets",
            format!("{} not in actions.allow_outlets allowlist", cmd.target()),
        ));
    }
    Ok(())
}

fn now_unix() -> i64 {
    zensight_common::current_timestamp_millis() / 1000
}

fn refused(cmd: &OutletAction, refusal: &Refusal) -> OutletStatus {
    OutletStatus {
        device: cmd.device.clone(),
        outlet: cmd.outlet.clone(),
        verb: cmd.verb,
        accepted: false,
        refused_by: Some(refusal.switch.to_string()),
        reason: Some(refusal.message.clone()),
        state_before: None,
        state_after: None,
        reboot_duration_secs: None,
        error: None,
        ts_unix: now_unix(),
    }
}

/// A bounded ring of recent outcomes, served on `@rpc/snmp/actions`.
///
/// In memory and lost on restart — the durable trail is #957's, on the host's
/// own audit subsystem. This is the operator timeline: what happened here,
/// lately, without leaving the GUI.
#[derive(Clone)]
pub struct History {
    ring: Arc<Mutex<VecDeque<OutletStatus>>>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.max(1)))),
            capacity: capacity.max(1),
        }
    }

    pub async fn record(&self, status: OutletStatus) {
        let mut ring = self.ring.lock().await;
        ring.push_front(status);
        while ring.len() > self.capacity {
            ring.pop_back();
        }
    }

    /// The most recent outcome, or `None`.
    ///
    /// `Option` deliberately: an all-empty `OutletStatus` reads exactly like a
    /// refusal, so "nothing has run" must be a different value on the wire.
    pub async fn last(&self) -> Option<OutletStatus> {
        self.ring.lock().await.front().cloned()
    }

    pub async fn recent(&self) -> Vec<OutletStatus> {
        self.ring.lock().await.iter().cloned().collect()
    }
}

/// Everything the serving task needs.
pub struct ActionServer {
    pub cfg: ActionsConfig,
    /// A copy of each controllable device with its **write** credential
    /// substituted in. Built once at startup so no request can reach a SET
    /// carrying the read credential.
    pub write_devices: Vec<DeviceConfig>,
    /// The devices as configured, for the gate's profile check.
    pub devices: Vec<DeviceConfig>,
}

/// Substitute the write credential into every device, once, at startup.
///
/// The read credential is not merely *preferred against* here — it is **not
/// present** in the value the SET path can reach. A gate that has to remember
/// which credential to use is a gate that will one day forget.
pub fn write_devices(cfg: &SnmpConfig) -> Vec<DeviceConfig> {
    let Some(set) = cfg
        .actions
        .credentials
        .as_deref()
        .and_then(|n| cfg.credentials.get(n))
    else {
        return Vec::new();
    };
    cfg.devices
        .iter()
        .map(|d| {
            let mut d = d.clone();
            if let Some(community) = &set.community {
                d.community = community.clone();
            }
            if let Some(security) = &set.security {
                d.security = Some(security.clone());
                d.version = crate::config::SnmpVersion::V3;
            }
            d
        })
        .collect()
}

impl ActionServer {
    /// Carry out one already-gated cycle.
    ///
    /// Reads the outlet's state before and after, and the PDU's own reboot
    /// duration where it publishes one. The "after" state is evidence the SET
    /// was **accepted**, not evidence the load restarted: a cycle is
    /// asynchronous inside the PDU and the outlet is usually still `on` a
    /// moment later. `OutletStatus`'s field docs say so, so nobody reads more
    /// into it than it carries.
    pub async fn execute(&self, cmd: &OutletAction) -> OutletStatus {
        let mut status = OutletStatus {
            device: cmd.device.clone(),
            outlet: cmd.outlet.clone(),
            verb: cmd.verb,
            accepted: true,
            refused_by: None,
            reason: None,
            state_before: None,
            state_after: None,
            reboot_duration_secs: None,
            error: None,
            ts_unix: now_unix(),
        };

        let Some(device) = self.write_devices.iter().find(|d| d.name == cmd.device) else {
            status.error = Some("no write credential for this device".to_string());
            return status;
        };
        let client = match crate::poller::build_probe_client(device).await {
            Ok(c) => c,
            Err(e) => {
                status.error = Some(format!("connect: {e}"));
                return status;
            }
        };

        let state_oid = format!("{APC_STATUS_STATE}.{}", cmd.outlet);
        let command_oid = format!("{APC_CONTROL_COMMAND}.{}", cmd.outlet);
        let duration_oid = format!("{APC_REBOOT_DURATION}.{}", cmd.outlet);

        status.state_before = read_state(&client, &state_oid).await;
        status.reboot_duration_secs = read_u32(&client, &duration_oid).await;

        let Ok(oid) = Oid::parse(&command_oid) else {
            status.error = Some(format!("not a valid OID: {command_oid}"));
            return status;
        };
        if let Err(e) = client.set(&oid, Value::Integer(APC_IMMEDIATE_REBOOT)).await {
            status.error = Some(format!("set: {e}"));
            return status;
        }
        status.state_after = read_state(&client, &state_oid).await;
        status
    }
}

async fn read_state(
    client: &async_snmp::Client<async_snmp::UdpHandle>,
    oid: &str,
) -> Option<String> {
    let value = read_value(client, oid).await?;
    match value {
        // off(1) / on(2) — APC's spelling, mapped once, here.
        Value::Integer(1) => Some("off".to_string()),
        Value::Integer(2) => Some("on".to_string()),
        other => Some(format!("{other:?}")),
    }
}

async fn read_u32(client: &async_snmp::Client<async_snmp::UdpHandle>, oid: &str) -> Option<u32> {
    match read_value(client, oid).await? {
        Value::Integer(n) => u32::try_from(n).ok(),
        Value::Gauge32(n) | Value::UInteger32(n) => Some(n),
        _ => None,
    }
}

async fn read_value(
    client: &async_snmp::Client<async_snmp::UdpHandle>,
    oid: &str,
) -> Option<Value> {
    let parsed = Oid::parse(oid).ok()?;
    // 0.18: a GET answers with the response's shape — the one binding asked
    // for, or an anomaly the crate has already classified. No binding is "no
    // value", which for these two reads is a PDU that does not publish them.
    client
        .get(&parsed)
        .await
        .ok()
        .and_then(|resp| resp.single().map(|vb| vb.value.clone()))
}

/// Serve the control channel until the session closes.
///
/// `action/capability` is served **first and unconditionally**, because "off"
/// has to be an answer rather than a silence (#648): a caller must be able to
/// tell a read-only deployment from an offline one without guessing.
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    server: ActionServer,
    history: History,
) {
    let cmd_key = command_key(&producer, ACTION_TOPIC);
    let stat_key = status_key(&producer, ACTION_TOPIC);
    let cap_key = nested_query_key(&producer, ACTION_TOPIC, "capability");
    let ring_key = status_key(&producer, ACTIONS_TOPIC);

    // The write half goes through the AUDITED seam (#957): there is no way to
    // answer `action/set` without both outcomes reaching the host's audit
    // trail. That is not a nicety here — it is the only record that a request
    // to cut power was ever made.
    let set_q = match zensight_common::served::serve_write_queryable(&session, &cmd_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "snmp: declare action/set failed");
            return;
        }
    };
    let (cap_q, stat_q, ring_q) = match (
        zensight_common::served::serve_queryable(&session, &cap_key).await,
        zensight_common::served::serve_queryable(&session, &stat_key).await,
        zensight_common::served::serve_queryable(&session, &ring_key).await,
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        _ => {
            tracing::error!("snmp: declaring the action read surfaces failed");
            return;
        }
    };

    if server.cfg.enabled {
        tracing::warn!(
            commands = %cmd_key,
            allow = ?server.cfg.allow_outlets,
            "snmp: gated PDU outlet control ENABLED — this sensor can now cut power to the \
             outlets on that allowlist. The bus caller is anonymous; every attempt is \
             recorded (#957), and no attempt is attributed"
        );
    }

    loop {
        tokio::select! {
            query = set_q.recv_async() => {
                let Ok(query) = query else { return };
                handle_set(&query_ctx(&server, &history), query, &cmd_key).await;
            }
            query = cap_q.recv_async() => {
                let Ok(query) = query else { return };
                reply_json(&query, &cap_key, &capability(&server.cfg)).await;
            }
            query = stat_q.recv_async() => {
                let Ok(query) = query else { return };
                reply_json(&query, &stat_key, &history.last().await).await;
            }
            query = ring_q.recv_async() => {
                let Ok(query) = query else { return };
                reply_json(&query, &ring_key, &history.recent().await).await;
            }
        }
    }
}

struct Ctx<'a> {
    server: &'a ActionServer,
    history: &'a History,
}

fn query_ctx<'a>(server: &'a ActionServer, history: &'a History) -> Ctx<'a> {
    Ctx { server, history }
}

async fn handle_set(ctx: &Ctx<'_>, query: WriteQuery, reply_key: &str) {
    let req = query.request();
    let cmd = match serde_json::from_slice::<OutletAction>(&req.payload) {
        Ok(cmd) => cmd,
        Err(e) => {
            tracing::warn!(error = %e, "snmp: bad outlet command");
            let err = zensight_sensor_core::rpc::RpcError::invalid_args(e.to_string());
            let _ = query.refused(&err, None).await;
            return;
        }
    };

    // Gate BEFORE anything reaches the network: a refusal costs nothing, and
    // it must be recorded even when the PDU is unreachable.
    if let Err(refusal) = gate(&ctx.server.cfg, &ctx.server.devices, &cmd) {
        ctx.history.record(refused(&cmd, &refusal)).await;
        let err = zensight_sensor_core::rpc::RpcError::gated(refusal.message.clone())
            .with_refused_by(refusal.switch);
        let _ = query.refused(&err, Some(&cmd.target())).await;
        return;
    }

    let status = ctx.server.execute(&cmd).await;
    ctx.history.record(status.clone()).await;
    match serde_json::to_vec(&status) {
        Ok(body) => {
            // `executed_but`, not `executed`: the gate said yes and the sensor
            // acted, but a SET that failed on the wire must not read as a
            // clean success in the operator's trail.
            if let Err(e) = query
                .executed_but(reply_key, body, Some(&cmd.target()), status.error.clone())
                .await
            {
                tracing::warn!(error = %e, "snmp: action reply failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "snmp: serialize outcome failed");
            let err =
                zensight_sensor_core::rpc::RpcError::new("error/snmp/serialize", e.to_string());
            let _ = query.refused(&err, Some(&cmd.target())).await;
        }
    }
}

async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, key: &str, body: &T) {
    match serde_json::to_vec(body) {
        Ok(payload) => {
            if let Err(e) = query.reply(key, payload).await {
                tracing::warn!(error = %e, key = %key, "snmp: action reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "snmp: serialize action reply failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SnmpVersion;

    fn device(name: &str, profile: Option<&str>) -> DeviceConfig {
        DeviceConfig {
            name: name.to_string(),
            address: "10.0.0.5:161".to_string(),
            community: "public".to_string(),
            version: SnmpVersion::V3,
            security: None,
            poll_interval_secs: 60,
            timeout_secs: 5,
            retries: 2,
            max_repetitions: 20,
            max_pdus_per_sec: None,
            max_concurrent: None,
            oids: Vec::new(),
            walks: Vec::new(),
            oid_group: None,
            alerts: None,
            profile: profile.map(str::to_string),
            credentials: None,
        }
    }

    fn cfg(enabled: bool, allow: &[&str]) -> ActionsConfig {
        ActionsConfig {
            enabled,
            allow_outlets: allow.iter().map(|s| s.to_string()).collect(),
            credentials: Some("write".to_string()),
            history_capacity: 8,
        }
    }

    fn act(device: &str, outlet: &str) -> OutletAction {
        OutletAction {
            device: device.to_string(),
            outlet: outlet.to_string(),
            verb: OutletVerb::Cycle,
        }
    }

    fn devices() -> Vec<DeviceConfig> {
        vec![
            device("pdu-a", Some("pdu-apc")),
            device("pdu-e", Some("pdu-eaton")),
            device("switch01", None),
        ]
    }

    /// **The default configuration cannot act.** Everything else in this file
    /// is secondary to that.
    #[test]
    fn the_default_configuration_refuses_everything() {
        let d = devices();
        let default = ActionsConfig::default();
        assert!(!default.enabled);
        assert!(default.allow_outlets.is_empty());
        assert!(default.credentials.is_none());
        let refusal = gate(&default, &d, &act("pdu-a", "3")).unwrap_err();
        assert_eq!(refusal.switch, "actions.enabled");
    }

    /// The master switch beats every other gate, including a permissive
    /// allowlist.
    #[test]
    fn the_master_switch_beats_every_other_gate() {
        let d = devices();
        let refusal = gate(&cfg(false, &["*"]), &d, &act("pdu-a", "3")).unwrap_err();
        assert_eq!(refusal.switch, "actions.enabled");
    }

    /// On, with an empty allowlist: still nothing, and the refusal names the
    /// allowlist rather than the master switch.
    #[test]
    fn an_empty_allowlist_refuses_and_names_itself() {
        let d = devices();
        let refusal = gate(&cfg(true, &[]), &d, &act("pdu-a", "3")).unwrap_err();
        assert_eq!(refusal.switch, "actions.allow_outlets");
        assert!(refusal.message.contains("pdu-a/3"), "{}", refusal.message);
    }

    /// There is no `allow_all`. A wildcard is something an operator typed.
    #[test]
    fn the_allowlist_scopes_to_the_outlet_not_just_the_device() {
        let d = devices();
        let c = cfg(true, &["pdu-a/3", "pdu-a/4"]);
        assert!(gate(&c, &d, &act("pdu-a", "3")).is_ok());
        assert!(gate(&c, &d, &act("pdu-a", "4")).is_ok());
        let refusal = gate(&c, &d, &act("pdu-a", "5")).unwrap_err();
        assert_eq!(refusal.switch, "actions.allow_outlets");
    }

    /// **A profile whose control OIDs were never verified cannot be
    /// controlled**, however permissive the allowlist. Guessing a control OID
    /// is categorically worse than guessing a read one: a wrong read publishes
    /// a wrong number, a wrong write does something to a machine.
    #[test]
    fn an_unverified_pdu_profile_is_refused_however_permissive_the_allowlist() {
        let d = devices();
        let c = cfg(true, &["*"]);
        assert!(gate(&c, &d, &act("pdu-a", "3")).is_ok(), "apc is verified");

        let refusal = gate(&c, &d, &act("pdu-e", "3")).unwrap_err();
        assert_eq!(refusal.switch, "devices[].profile");
        assert!(
            refusal.message.contains("pdu-apc"),
            "the supported list is named: {}",
            refusal.message
        );

        let refusal = gate(&c, &d, &act("switch01", "3")).unwrap_err();
        assert_eq!(refusal.switch, "devices[].profile");
    }

    /// A device this sensor does not poll cannot be targeted at all.
    #[test]
    fn an_unknown_device_is_refused() {
        let refusal = gate(
            &cfg(true, &["*"]),
            &devices(),
            &act("someone-elses-pdu", "1"),
        )
        .unwrap_err();
        assert_eq!(refusal.switch, "snmp.devices");
    }

    /// The read credential is not merely deprioritised on the SET path — it is
    /// not present in the value that path can reach.
    #[test]
    fn the_write_device_set_carries_the_write_credential_and_not_the_read_one() {
        use crate::config::CredentialSet;
        // The whole config, from JSON5, so the shape a deployment writes is
        // the shape this is tested against.
        let mut snmp: SnmpConfig = json5::from_str(
            r#"{
                allow_insecure_versions: true,
                devices: [],
                credentials: {
                    write: { community: "write-community" },
                },
                actions: {
                    enabled: true,
                    allow_outlets: ["pdu-a/*"],
                    credentials: "write",
                },
            }"#,
        )
        .expect("the config shape parses");
        let mut d = device("pdu-a", Some("pdu-apc"));
        d.community = "read-only-community".to_string();
        snmp.devices = vec![d];
        assert!(snmp.credentials.contains_key("write"), "sanity");
        let _ = CredentialSet {
            community: None,
            security: None,
        };

        let devices = write_devices(&snmp);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].community, "write-community");
        assert_ne!(devices[0].community, "read-only-community");

        // No credential set named: nothing to act with, so nothing is built.
        snmp.actions.credentials = None;
        assert!(write_devices(&snmp).is_empty());
    }

    /// The advertised capability must agree with the gate that actually runs —
    /// the whole point of publishing it is that a caller can trust the preview.
    #[test]
    fn advertised_capability_agrees_with_the_gate() {
        let d = devices();
        for c in [
            cfg(false, &["pdu-a/3"]),
            cfg(true, &[]),
            cfg(true, &["pdu-a/3"]),
            cfg(true, &["pdu-a/*"]),
        ] {
            let cap = capability(&c);
            for outlet in ["3", "4"] {
                let cmd = act("pdu-a", outlet);
                assert_eq!(
                    cap.permits(&cmd.target()),
                    gate(&c, &d, &cmd).is_ok(),
                    "{} disagreed (enabled={}, allow={:?})",
                    cmd.target(),
                    c.enabled,
                    c.allow_outlets
                );
            }
        }
    }

    /// A refusal has to say which switch refused (#866) AND name the file to
    /// edit — the reason otherwise lives only in a log on a machine the
    /// operator is not reading.
    #[test]
    fn a_refusal_says_why_and_a_permission_does_not() {
        let off = capability(&ActionsConfig::default());
        let why = off.reason.as_deref().expect("a disabled host says why");
        assert!(why.contains("actions.enabled"), "names the switch: {why}");
        assert!(
            why.contains("configs/snmp.json5"),
            "names the file to edit: {why}"
        );

        let empty = capability(&cfg(true, &[]));
        let why = empty
            .reason
            .as_deref()
            .expect("an empty allowlist says why");
        assert!(why.contains("allow_outlets"), "{why}");
        assert!(empty.enabled, "the switch really is on");

        assert_eq!(capability(&cfg(true, &["pdu-a/3"])).reason, None);
    }

    /// Every gate arm names a switch that is a real field of the config an
    /// operator would edit. A plausible name no key matches sends them looking
    /// for a setting that does not exist.
    #[test]
    fn every_gate_arm_names_something_an_operator_can_edit() {
        let known = [
            "actions.enabled",
            "actions.allow_outlets",
            "actions.credentials",
            "devices[].profile",
            "snmp.devices",
        ];
        let d = devices();
        let mut seen = std::collections::HashSet::new();
        let mut no_creds = cfg(true, &["*"]);
        no_creds.credentials = None;
        for (c, cmd) in [
            (cfg(false, &["*"]), act("pdu-a", "3")),
            (no_creds, act("pdu-a", "3")),
            (cfg(true, &[]), act("pdu-a", "3")),
            (cfg(true, &["*"]), act("pdu-e", "3")),
            (cfg(true, &["*"]), act("nope", "3")),
            (cfg(true, &["*"]), act("pdu-a", "")),
        ] {
            let refusal = gate(&c, &d, &cmd).expect_err("this configuration must refuse");
            assert!(
                known.contains(&refusal.switch),
                "{:?} is not a config key an operator can edit",
                refusal.switch
            );
            // The sentence names its own switch too, for the caller that
            // only ever sees the message.
            assert!(
                refusal.message.contains(refusal.switch),
                "the sentence must name the switch too: {refusal:?}"
            );
            seen.insert(refusal.switch);
        }
        assert_eq!(seen.len(), known.len(), "an arm went untested: {seen:?}");
    }

    /// The ring is bounded, newest first, and answers `None` — not an
    /// all-empty status, which reads exactly like a refusal — before anything
    /// has run.
    #[tokio::test]
    async fn the_ring_is_bounded_and_says_nothing_rather_than_nothing_happened() {
        let h = History::new(2);
        assert!(h.last().await.is_none());
        for outlet in ["1", "2", "3"] {
            h.record(refused(
                &act("pdu-a", outlet),
                &Refusal::new("actions.enabled", "off"),
            ))
            .await;
        }
        let recent = h.recent().await;
        assert_eq!(recent.len(), 2, "bounded");
        assert_eq!(recent[0].outlet, "3", "newest first");
        assert_eq!(h.last().await.unwrap().outlet, "3");
    }

    /// **The end-to-end claim**: a default deployment declares the write
    /// procedure, answers it, and refuses — and the capability probe says so
    /// before anyone tries.
    ///
    /// Both halves matter. Without the reply, a caller cannot tell a
    /// read-only host from an offline one and has to guess whether to offer
    /// the control. Without the refusal, the gate is a comment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_default_sensor_answers_the_probe_and_refuses_the_write_surface() {
        // Unique prefix so parallel test runs do not cross-talk.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let producer = format!("test-{nanos}-snmp");

        // Multicast scouting OFF. A default-config session joins whatever mesh
        // it can reach — including a live fleet on the same host — so a test
        // that scouts is not a test, it is a participant (RFC 09 §0.1).
        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .expect("disable multicast scouting");
        let session = Arc::new(zenoh::open(config).await.expect("open zenoh session"));

        let server = ActionServer {
            cfg: ActionsConfig::default(),
            write_devices: Vec::new(),
            devices: devices(),
        };
        let handle = tokio::spawn(run(
            session.clone(),
            producer.clone(),
            server,
            History::new(8),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // The gate is legible BEFORE anyone clicks.
        let replies = session
            .get(&nested_query_key(&producer, ACTION_TOPIC, "capability"))
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get capability");
        let reply = replies.recv_async().await.expect("the probe must answer");
        let sample = reply.result().expect("ok reply");
        let cap: OutletCapability =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("decode capability");
        assert!(!cap.enabled, "a read-only deployment says so out loud");
        assert!(cap.allow_outlets.is_empty());
        assert!(
            cap.reason
                .as_deref()
                .is_some_and(|r| r.contains("actions.enabled")),
            "and names the switch: {:?}",
            cap.reason
        );

        // …and the write procedure IS declared, answering `error/gated`.
        // Declaring it exposes nothing — the queryable executes nothing and
        // only reports that control is off, which the capability above already
        // broadcasts. What a caller gains is the difference between "no snmp
        // sensor answered" and "this deployment will not do that" (#648).
        let body = serde_json::to_vec(&act("pdu-a", "3")).unwrap();
        let replies = session
            .get(&command_key(&producer, ACTION_TOPIC))
            .payload(body)
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get action/set");
        let reply = replies
            .recv_async()
            .await
            .expect("a disabled sensor still answers its declared write procedure");
        let err = reply
            .result()
            .expect_err("a disabled sensor must refuse, not accept");
        let decoded: zensight_common::rpc::RpcError =
            serde_json::from_slice(&err.payload().to_bytes()).expect("decode RpcError");
        assert_eq!(
            decoded.error,
            zensight_common::rpc::ERR_GATED,
            "the refusal must say `gated` (built in, switched off) rather than `unsupported` \
             (absent from the build) — they call for different fixes"
        );
        assert_eq!(
            decoded.refused_by.as_deref(),
            Some("actions.enabled"),
            "the switch reaches the CALLER as a field too (#866/#957), not only the trail"
        );

        handle.abort();
    }

    /// The control OIDs, pinned. They were read out of PowerNet-MIB v4.5.8;
    /// if one moves, it moves here and in the vendor's MIB, not by accident.
    #[test]
    fn the_apc_control_oids_are_the_ones_from_the_mib() {
        assert_eq!(APC_CONTROL_COMMAND, "1.3.6.1.4.1.318.1.1.26.9.2.4.1.5");
        assert_eq!(APC_IMMEDIATE_REBOOT, 3);
        assert_eq!(APC_STATUS_STATE, "1.3.6.1.4.1.318.1.1.26.9.2.3.1.5");
        assert_eq!(APC_REBOOT_DURATION, "1.3.6.1.4.1.318.1.1.26.9.2.1.1.7");
    }
}
