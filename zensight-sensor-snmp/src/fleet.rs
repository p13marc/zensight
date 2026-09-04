//! The device set, and changing it without a restart (#936).
//!
//! Before this, `main.rs` spawned one poller per configured device in a `for`
//! loop and kept no handle to any of them. Adding a device meant editing a
//! file on that host and restarting the sensor — which is the eighteen-files
//! problem in its most acute form, because an SNMP device is exactly the kind
//! of thing a fleet operator adds at three in the afternoon.
//!
//! # Why abort rather than a graceful stop flag
//!
//! A removed device's poller is aborted mid-flight. That loses at most one
//! in-flight GET, and an SNMP GET is idempotent and stateless — there is
//! nothing half-written to leave behind. The alternative, a stop flag each
//! poll loop checks, would hold a removed device's task alive for up to its
//! poll interval (which an operator may have set to an hour) while it
//! continued to query a device they just said to stop querying. For a monitor
//! that is the wrong way round.
//!
//! # What a "change" is
//!
//! A device is respawned when **anything** about it changes, not just its
//! address: the poller reads its whole `DeviceConfig` at construction — oids,
//! walks, profile, intervals, credentials — so a partial update would leave a
//! poller running against a config nobody can see any more. Comparing the
//! serialized form is the cheap way to be exhaustive about a struct with
//! seventeen fields, and it cannot drift when an eighteenth is added.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::config::DeviceConfig;

/// Builds and starts one device poller. Everything a poller needs — session,
/// MIB resolver, thresholds, health, the shared known-IP set — is captured
/// here once, so the supervisor stays ignorant of it.
/// Synchronous on purpose: it *starts* a poller and hands back the handle.
///
/// The client's `init()` happens inside the spawned task rather than before
/// it, which it can because an init failure was already non-fatal — the poll
/// loop retries with backoff, so a device that is offline at startup starts
/// working when it comes online (#539). Doing it inside means this returns a
/// handle rather than a future that yields one, and the supervisor needs no
/// boxed future at all.
pub type SpawnDevice = Arc<dyn Fn(DeviceConfig) -> JoinHandle<()> + Send + Sync>;

/// The running device pollers, keyed by device name.
pub struct DeviceFleet {
    spawn: SpawnDevice,
    running: HashMap<String, Running>,
}

struct Running {
    /// The serialized config this poller was started with — the change
    /// detector. See the module header on why it is the whole struct.
    fingerprint: String,
    /// The config it is running with, so `@rpc/snmp/targets` can answer with
    /// what is *actually* being polled rather than with what the config file
    /// said at startup. Those differ the moment a set is replaced, and a read
    /// procedure that reported the second would be answering a question
    /// nobody asked.
    device: DeviceConfig,
    handle: JoinHandle<()>,
}

/// What one `apply` did, for the log line and for the tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FleetChange {
    pub added: Vec<String>,
    pub restarted: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

impl FleetChange {
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.restarted.is_empty() && self.removed.is_empty()
    }
}

impl DeviceFleet {
    pub fn new(spawn: SpawnDevice) -> Self {
        DeviceFleet {
            spawn,
            running: HashMap::new(),
        }
    }

    /// How many pollers are running. Feeds `health().set_devices_total`, which
    /// was a startup-fixed number before this and would otherwise now be a
    /// lie.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.running.keys().cloned().collect();
        v.sort();
        v
    }

    /// The configs currently being polled, by name.
    pub fn devices(&self) -> Vec<DeviceConfig> {
        let mut v: Vec<DeviceConfig> = self.running.values().map(|r| r.device.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Make the running set match `devices`.
    ///
    /// Deterministic in its reporting: sorted, so two runs over the same
    /// change log the same line and a test can assert on it.
    pub fn apply(&mut self, devices: &[DeviceConfig]) -> FleetChange {
        let mut change = FleetChange::default();

        let mut wanted: Vec<(String, String, DeviceConfig)> = devices
            .iter()
            .map(|d| (d.name.clone(), fingerprint(d), d.clone()))
            .collect();
        wanted.sort_by(|a, b| a.0.cmp(&b.0));

        let want_names: std::collections::HashSet<&str> =
            wanted.iter().map(|(n, _, _)| n.as_str()).collect();

        // Remove first, so a device that moved address does not have two
        // pollers querying it even briefly.
        let gone: Vec<String> = self
            .running
            .keys()
            .filter(|n| !want_names.contains(n.as_str()))
            .cloned()
            .collect();
        for name in gone {
            if let Some(r) = self.running.remove(&name) {
                r.handle.abort();
                change.removed.push(name);
            }
        }
        change.removed.sort();

        for (name, fp, device) in wanted {
            match self.running.get(&name) {
                Some(r) if r.fingerprint == fp => {
                    change.unchanged += 1;
                    continue;
                }
                Some(_) => {
                    if let Some(r) = self.running.remove(&name) {
                        r.handle.abort();
                    }
                    change.restarted.push(name.clone());
                }
                None => change.added.push(name.clone()),
            }
            let handle = (self.spawn)(device.clone());
            self.running.insert(
                name,
                Running {
                    fingerprint: fp,
                    device,
                    handle,
                },
            );
        }
        change.added.sort();
        change.restarted.sort();
        change
    }

    /// Stop every poller. Called on shutdown, so a removed device does not
    /// outlive the process that was asked to stop.
    pub fn abort_all(&mut self) {
        for (_, r) in self.running.drain() {
            r.handle.abort();
        }
    }
}

/// The change detector: the device's whole serialized config.
///
/// `DeviceConfig`'s `Debug` redacts credentials (#538) and would therefore
/// make two devices differing only in password look identical — which is
/// exactly the change that must restart a poller. Serializing does not redact,
/// and this string never leaves the process.
fn fingerprint(d: &DeviceConfig) -> String {
    serde_json::to_string(d).unwrap_or_else(|_| format!("{}|unserializable", d.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Built through serde rather than a `Default` derive: `DeviceConfig` is
    /// a shipped config type and giving it a `Default` would invite one.
    fn device(name: &str, address: &str) -> DeviceConfig {
        serde_json::from_value(serde_json::json!({ "name": name, "address": address }))
            .expect("a device with just a name and an address is valid")
    }

    /// A counting spawner: each poller parks forever, so an abort is the only
    /// way one ends and the count is exact.
    fn counting() -> (SpawnDevice, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let seen = n.clone();
        let spawn: SpawnDevice = Arc::new(move |_d: DeviceConfig| {
            seen.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(std::future::pending::<()>())
        });
        (spawn, n)
    }

    #[tokio::test]
    async fn an_unchanged_set_spawns_nothing() {
        let (spawn, spawned) = counting();
        let mut fleet = DeviceFleet::new(spawn);
        let set = vec![device("sw1", "10.0.0.1"), device("sw2", "10.0.0.2")];

        let first = fleet.apply(&set);
        assert_eq!(first.added, vec!["sw1", "sw2"]);
        assert_eq!(spawned.load(Ordering::SeqCst), 2);

        let second = fleet.apply(&set);
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(second.unchanged, 2);
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            2,
            "re-applying the same set must not restart a poller — a reconcile \
             happens on every reconnect, and restarting every device each time \
             would make the sensor useless on a flapping link"
        );
    }

    #[tokio::test]
    async fn a_changed_device_is_restarted_and_a_gone_one_is_stopped() {
        let (spawn, spawned) = counting();
        let mut fleet = DeviceFleet::new(spawn);
        fleet.apply(&[device("sw1", "10.0.0.1"), device("sw2", "10.0.0.2")]);

        let change = fleet.apply(&[device("sw1", "10.0.0.9"), device("sw3", "10.0.0.3")]);
        assert_eq!(change.restarted, vec!["sw1"], "address changed");
        assert_eq!(change.removed, vec!["sw2"]);
        assert_eq!(change.added, vec!["sw3"]);
        assert_eq!(spawned.load(Ordering::SeqCst), 4, "two more spawns");
        assert_eq!(fleet.names(), vec!["sw1", "sw3"]);
    }

    /// The poller reads the WHOLE device config at construction, so any field
    /// changing has to restart it. A comparison that only watched the address
    /// would leave a poller running with an interval, a profile or a
    /// credential nobody can see any more.
    #[tokio::test]
    async fn a_change_anywhere_in_the_config_restarts_the_poller() {
        let (spawn, _) = counting();
        let mut fleet = DeviceFleet::new(spawn);
        let mut d = device("sw1", "10.0.0.1");
        fleet.apply(std::slice::from_ref(&d));

        d.poll_interval_secs = 120;
        let change = fleet.apply(std::slice::from_ref(&d));
        assert_eq!(change.restarted, vec!["sw1"], "interval is part of it");

        d.community = "changed".into();
        let change = fleet.apply(std::slice::from_ref(&d));
        assert_eq!(
            change.restarted,
            vec!["sw1"],
            "and so is a credential — Debug redacts it, which is why the \
             fingerprint serializes instead"
        );
    }

    #[tokio::test]
    async fn an_empty_set_stops_everything() {
        let (spawn, _) = counting();
        let mut fleet = DeviceFleet::new(spawn);
        fleet.apply(&[device("sw1", "10.0.0.1")]);
        let change = fleet.apply(&[]);
        assert_eq!(change.removed, vec!["sw1"]);
        assert!(fleet.is_empty());
    }
}
