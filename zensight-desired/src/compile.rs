//! Fleet + policy → the documents that should be on the bus (#938).
//!
//! Pure: no session, no clock, no filesystem. That is what lets `plan` run on
//! a laptop against a fixture fleet and produce exactly what `apply` would
//! publish — and what makes the golden-fixture test in this module a real test
//! of the compiler rather than of a mock.

use std::collections::{BTreeMap, BTreeSet};

use zensight_common::HostEntity;

use crate::merge::{canonical, overlay};
use crate::policy::Policy;

/// One document the policy says a host should have.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    /// The host chunk the key is built with — `HostEntity::host_id`.
    pub host: String,
    pub producer: String,
    pub topic: String,
    pub doc: serde_json::Value,
    /// Canonical bytes, for the content diff that decides whether to publish.
    pub canonical: String,
}

/// What compiling produced, including what it refused.
#[derive(Debug, Default)]
pub struct Compiled {
    /// Keyed `(host, producer, topic)` so the output order is deterministic
    /// and a diff between two runs is readable.
    pub docs: BTreeMap<(String, String, String), Desired>,
    /// Documents that did not survive validation, with the reason. **Not**
    /// fatal to the whole pass: one host's bad override must not stop the
    /// other forty from converging, and the alternative — publish nothing —
    /// is the failure mode with no upper bound on its blast radius.
    pub rejected: Vec<String>,
    /// Entities the policy matched no class for. Reported, because "I wrote a
    /// class for these and it selects nobody" is the mistake this daemon is
    /// most likely to hide.
    pub unmatched: Vec<String>,
    /// The host ids this pass actually saw in the catalog, whether or not the
    /// policy yielded anything for them.
    ///
    /// **This is what makes deletion safe.** A document may be removed only
    /// when the policy stopped yielding it for a host that is *still here*;
    /// a host that vanished from the catalog is a host the daemon knows
    /// nothing about, and "I cannot see it" must never be read as "it should
    /// have no configuration". Without this set, one failed catalog GET
    /// empties `docs`, and every document in the fleet becomes a deletion
    /// candidate at once.
    pub hosts_seen: BTreeSet<String>,
}

/// Compile the policy against a fleet.
///
/// Only entities with a `host_id` take part: the key's first subject chunk is
/// the target host id (RFC 07 §3 G1), and an entity the catalog fused from
/// weak evidence alone has none. Publishing under an entity id that is not a
/// host id would build a key no sensor reconciles.
pub fn compile(policy: &Policy, fleet: &[HostEntity]) -> Compiled {
    compile_with(policy, fleet, &crate::overrides::Overrides::default())
}

/// Compile with recorded per-host overrides applied **last** (#939).
///
/// After every class and after the policy's own `hosts` section: an explicit
/// adoption, made through `override/set` by someone who was looking at that
/// host, is the most specific statement there is.
pub fn compile_with(
    policy: &Policy,
    fleet: &[HostEntity],
    overrides: &crate::overrides::Overrides,
) -> Compiled {
    let mut out = Compiled::default();

    let mut sorted: Vec<&HostEntity> = fleet.iter().collect();
    sorted.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));

    for e in sorted {
        let Some(host) = e.host_id.as_deref() else {
            continue;
        };
        out.hosts_seen.insert(host.to_string());

        // `classes_for` already folds in whatever the host override
        // `extends`, expanded and deduped, so there is one list here and one
        // meaning for the word.
        let classes = policy.classes_for(e);
        let host_override = policy.override_for(e);

        if classes.is_empty() && host_override.is_none() && !overrides.hosts.contains_key(host) {
            out.unmatched.push(host.to_string());
            continue;
        }

        // Every `<producer>/<topic>` any applicable layer mentions.
        let mut keys: Vec<String> = Vec::new();
        for c in &classes {
            keys.extend(c.docs.keys().cloned());
        }
        if let Some(o) = host_override {
            keys.extend(o.docs.keys().cloned());
        }
        let adopted = overrides.for_host(host);
        keys.extend(adopted.keys().cloned());
        keys.sort();
        keys.dedup();

        for key in keys {
            let Some((producer, topic)) = key.split_once('/') else {
                continue;
            };
            let Some(spec) = zensight_common::desired::topic(producer, topic) else {
                continue; // `validate` already reported it
            };

            // Overlay order: every applicable class (matched ones in file
            // order, then the host's own `extends`, each preceded by what it
            // extends), then the override's own documents. Last writer wins a
            // field.
            let mut doc = serde_json::Value::Object(Default::default());
            let mut contributed = false;
            for c in classes.iter() {
                if let Some(frag) = c.docs.get(&key) {
                    doc = overlay(doc, frag.clone());
                    contributed = true;
                }
            }
            if let Some(frag) = host_override.and_then(|o| o.docs.get(&key)) {
                doc = overlay(doc, frag.clone());
                contributed = true;
            }
            if let Some(frag) = adopted.get(&key) {
                doc = overlay(doc, frag.clone());
                contributed = true;
            }
            if !contributed {
                continue;
            }

            // The merged document is what reaches the wire, so it is what gets
            // type-checked — a fragment may legitimately be half a document.
            if let Err(e) = spec.validate(&doc) {
                out.rejected.push(format!("{host} {key}: {e}"));
                continue;
            }

            let canonical = canonical(&doc);
            out.docs.insert(
                (host.to_string(), producer.to_string(), topic.to_string()),
                Desired {
                    host: host.to_string(),
                    producer: producer.to_string(),
                    topic: topic.to_string(),
                    doc,
                    canonical,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::entity::MemberClaim;

    fn entity(id: &str, hostname: &str, platform: Option<&str>, sensors: &[&str]) -> HostEntity {
        HostEntity {
            entity_id: format!("h_{id}"),
            aliases: Vec::new(),
            host_id: Some(format!("h-{id}")),
            boot_id: None,
            ips: vec!["10.0.0.7".into()],
            macs: Vec::new(),
            container_ids: Vec::new(),
            origins: vec![format!("h-{id}")],
            hostname: Some(hostname.into()),
            fqdn: None,
            names: Vec::new(),
            vendor: Some("Dell Inc.".into()),
            platform: platform.map(str::to_string),
            members: sensors
                .iter()
                .map(|s| MemberClaim {
                    sensor: (*s).into(),
                    source: hostname.into(),
                    rule: "host_id".into(),
                    confidence: 1.0,
                    last_seen: 0,
                })
                .collect(),
            status: None,
            last_updated: 0,
        }
    }

    const POLICY: &str = r#"{
      classes: [
        { name: "all-hosts",
          matches: { always: true },
          docs: {
            "sysinfo/thresholds": {
              rules: [
                { name: "disk-full", metric: "disk/used_pct", op: "GreaterThan", value: 90 },
              ],
            },
          },
        },
        { name: "hypervisors",
          extends: ["all-hosts"],
          matches: { any: [ { platform: "proxmox-*" } ] },
          docs: {
            "sysinfo/thresholds": {
              rules: [
                { name: "disk-full", value: 80 },
                { name: "load", metric: "load/avg1", op: "GreaterThan", value: 32 },
              ],
            },
          },
        },
      ],
      hosts: {
        "h-noisy": { docs: { "sysinfo/thresholds": { rules: [ { name: "load", value: 64 } ] } } },
      },
    }"#;

    fn policy() -> Policy {
        let p: Policy = json5::from_str(POLICY).expect("fixture policy parses");
        assert!(p.validate().is_empty(), "fixture policy: {}", p.validate());
        p
    }

    fn rules(c: &Compiled, host: &str) -> Vec<(String, f64)> {
        c.docs[&(host.into(), "sysinfo".into(), "thresholds".into())].doc["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap().to_string(),
                    r["value"].as_f64().unwrap(),
                )
            })
            .collect()
    }

    /// A plain host takes only what `all-hosts` gives it.
    #[test]
    fn a_matched_class_reaches_every_host() {
        let out = compile(
            &policy(),
            &[entity("web1", "web01", Some("debian-13"), &["sysinfo"])],
        );
        assert!(out.rejected.is_empty(), "{:?}", out.rejected);
        assert_eq!(rules(&out, "h-web1"), vec![("disk-full".into(), 90.0)]);
    }

    /// A hypervisor keeps the inherited rule, with the class's own value, and
    /// gains the one the class adds. This is the case list-replacement would
    /// have broken silently.
    #[test]
    fn extends_overlays_rather_than_replaces() {
        let out = compile(
            &policy(),
            &[entity("pve1", "pve01", Some("proxmox-13"), &["sysinfo"])],
        );
        assert_eq!(
            rules(&out, "h-pve1"),
            vec![("disk-full".into(), 80.0), ("load".into(), 32.0)]
        );
        // And the inherited rule kept the fields the override did not mention.
        let doc = &out.docs[&("h-pve1".into(), "sysinfo".into(), "thresholds".into())].doc;
        assert_eq!(doc["rules"][0]["metric"], "disk/used_pct");
    }

    /// `proxmox-*` is a glob on purpose: `platform` carries the version since
    /// #935, so an exact `proxmox` would match nothing and no error would say
    /// so.
    #[test]
    fn the_platform_selector_globs_the_version() {
        let out = compile(
            &policy(),
            &[entity("pve2", "pve02", Some("proxmox-14"), &["sysinfo"])],
        );
        assert_eq!(
            rules(&out, "h-pve2").len(),
            2,
            "a later Proxmox still matches"
        );
    }

    #[test]
    fn a_host_override_wins_over_every_class() {
        let out = compile(
            &policy(),
            &[entity("noisy", "noisy01", Some("proxmox-13"), &["sysinfo"])],
        );
        assert_eq!(
            rules(&out, "h-noisy"),
            vec![("disk-full".into(), 80.0), ("load".into(), 64.0)]
        );
    }

    /// `extends` on a host means what it means on a class — transitively.
    ///
    /// It expanded only one level before, so a host opting into `hypervisors`
    /// got that class's rules and **not** the `all-hosts` rules it is built on:
    /// the same word doing two things in one file, and the shortfall invisible
    /// (a well-formed document, one rule missing).
    #[test]
    fn a_host_extends_transitively_like_a_class_does() {
        let mut p = policy();
        p.hosts.insert(
            "h-plain".into(),
            crate::policy::HostOverride {
                extends: vec!["hypervisors".into()],
                docs: Default::default(),
            },
        );
        // A Debian host, so it matches NO class on its own — everything it
        // gets, it gets by asking.
        let out = compile(
            &p,
            &[entity("plain", "plain01", Some("debian-13"), &["sysinfo"])],
        );
        assert!(out.rejected.is_empty(), "{:?}", out.rejected);
        assert_eq!(
            rules(&out, "h-plain"),
            vec![("disk-full".into(), 80.0), ("load".into(), 32.0)],
            "it gets all-hosts' rule at hypervisors' value, plus hypervisors' own"
        );
    }

    /// A class reached twice — once by matching, once by a host's `extends` —
    /// is applied once. Applying it twice is harmless for the overlay (it is
    /// idempotent) and confusing in `render`, which is the output an operator
    /// checks a policy against.
    #[test]
    fn a_class_reached_twice_is_applied_once() {
        let mut p = policy();
        p.hosts.insert(
            "h-pve1".into(),
            crate::policy::HostOverride {
                extends: vec!["hypervisors".into(), "all-hosts".into()],
                docs: Default::default(),
            },
        );
        let with = compile(
            &p,
            &[entity("pve1", "pve01", Some("proxmox-13"), &["sysinfo"])],
        );
        let without = compile(
            &policy(),
            &[entity("pve1", "pve01", Some("proxmox-13"), &["sysinfo"])],
        );
        assert_eq!(
            with.docs[&("h-pve1".into(), "sysinfo".into(), "thresholds".into())].canonical,
            without.docs[&("h-pve1".into(), "sysinfo".into(), "thresholds".into())].canonical,
            "asking for classes it already matches changes nothing"
        );
    }

    /// An adoption overlays after **everything** — every class, and the
    /// policy's own host section. Someone pressed a button while looking at
    /// that host; that is the most specific statement there is.
    #[test]
    fn an_override_overlays_last() {
        let mut ov = crate::overrides::Overrides::default();
        ov.apply(&zensight_common::desired::DesiredOverride {
            host: "h-noisy".into(),
            producer: "sysinfo".into(),
            topic: "thresholds".into(),
            doc: Some(serde_json::json!({ "rules": [{ "name": "load", "value": 128 }] })),
            by: Some("alice".into()),
            note: None,
            at: 0,
        });
        let out = compile_with(
            &policy(),
            &[entity("noisy", "noisy01", Some("proxmox-13"), &["sysinfo"])],
            &ov,
        );
        assert!(out.rejected.is_empty(), "{:?}", out.rejected);
        // The policy's own `hosts` section already set load to 64 here; the
        // adoption wins, and the inherited disk-full rule survives both.
        assert_eq!(
            rules(&out, "h-noisy"),
            vec![("disk-full".into(), 80.0), ("load".into(), 128.0)]
        );
    }

    /// A host no class matches still gets its adoption. Otherwise adopting a
    /// device on a machine the policy says nothing about — which is exactly
    /// the discovery case #940 is for — would silently do nothing.
    #[test]
    fn an_override_reaches_a_host_no_class_matches() {
        let mut p = policy();
        p.classes.clear();
        let mut ov = crate::overrides::Overrides::default();
        ov.apply(&zensight_common::desired::DesiredOverride {
            host: "h-lonely".into(),
            producer: "sysinfo".into(),
            topic: "thresholds".into(),
            // A WHOLE rule: with no class to inherit from, the adoption is
            // the entire document, and the compiler refuses a partial one —
            // which is what it should do, and what the first draft of this
            // test discovered the hard way.
            doc: Some(serde_json::json!({ "rules": [{
                "name": "disk-full", "metric": "disk/used_pct",
                "op": "GreaterThan", "value": 95
            }] })),
            by: None,
            note: None,
            at: 0,
        });
        let out = compile_with(
            &p,
            &[entity(
                "lonely",
                "lonely01",
                Some("debian-13"),
                &["sysinfo"],
            )],
            &ov,
        );
        assert_eq!(rules(&out, "h-lonely"), vec![("disk-full".into(), 95.0)]);
        assert!(out.unmatched.is_empty(), "adopted, so not unmatched");
    }

    /// The same inputs must produce the same bytes, or a restart rewrites the
    /// fleet.
    #[test]
    fn compiling_twice_produces_identical_bytes() {
        let fleet = [
            entity("pve1", "pve01", Some("proxmox-13"), &["sysinfo"]),
            entity("web1", "web01", Some("debian-13"), &["sysinfo"]),
        ];
        let a = compile(&policy(), &fleet);
        let mut reversed = fleet.clone();
        reversed.reverse();
        let b = compile(&policy(), &reversed);
        let ca: Vec<&String> = a.docs.values().map(|d| &d.canonical).collect();
        let cb: Vec<&String> = b.docs.values().map(|d| &d.canonical).collect();
        assert_eq!(ca, cb, "output must not depend on fleet iteration order");
    }

    /// One host's bad override must not stop the other hosts converging. The
    /// alternative — publish nothing — has no upper bound on its blast radius.
    #[test]
    fn a_rejected_document_does_not_take_the_pass_down_with_it() {
        let mut p = policy();
        p.hosts.insert(
            "h-bad".into(),
            crate::policy::HostOverride {
                extends: Vec::new(),
                docs: [(
                    "sysinfo/thresholds".to_string(),
                    serde_json::json!({ "rules": "not a list" }),
                )]
                .into_iter()
                .collect(),
            },
        );
        let out = compile(
            &p,
            &[
                entity("bad", "bad01", Some("debian-13"), &["sysinfo"]),
                entity("web1", "web01", Some("debian-13"), &["sysinfo"]),
            ],
        );
        assert_eq!(out.rejected.len(), 1);
        assert!(out.rejected[0].contains("h-bad"), "{:?}", out.rejected);
        assert!(
            out.docs
                .contains_key(&("h-web1".into(), "sysinfo".into(), "thresholds".into())),
            "the healthy host still converges"
        );
    }

    /// An entity the catalog fused from weak evidence has no `host_id`, so
    /// there is no key to publish under. Skipped, not guessed at.
    #[test]
    fn an_entity_without_a_host_id_is_skipped() {
        let mut e = entity("weak", "weak01", Some("debian-13"), &["sysinfo"]);
        e.host_id = None;
        let out = compile(&policy(), &[e]);
        assert!(out.docs.is_empty());
        assert!(out.unmatched.is_empty(), "not unmatched — ineligible");
    }
}
