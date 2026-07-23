//! Device profiles: curated OID sets matched by sysObjectID (#531).
//!
//! Onboarding a device should not require knowing which OIDs matter. A
//! profile declares what to poll (scalars + table walks) and how to name it
//! (lowercase, chunk-grammar-valid `oid_names` with `{index}` placeholders,
//! plus SYNTAX hints for the rate tracker). Profiles are TOML:
//!
//! ```toml
//! name = "cisco-switch"
//! extends = ["generic-device", "network-interfaces"]
//! oids  = ["1.3.6.1.2.1.1.5.0"]
//! walks = ["1.3.6.1.2.1.2.2.1.10"]
//!
//! [match]
//! sys_object_id = ["1.3.6.1.4.1.9."]   # OID prefixes; or `default = true`
//!
//! [oid_names]
//! "1.3.6.1.2.1.1.5.0" = "system/name"
//!
//! [oid_syntax]
//! "1.3.6.1.2.1.2.2.1.10" = "Counter32"
//! ```
//!
//! Four base profiles ship **embedded in the binary** (`profiles/`):
//! `generic-device`, `network-interfaces`, `host-resources`,
//! `entity-sensors` — the first two are `default = true` (every device gets
//! system + interface coverage out of the box); the resource/sensor tables
//! are polled only where matched or extended. User profiles load from
//! `snmp.profiles.dirs` and may override shipped ones by name; a broken
//! profile file fails startup loudly.
//!
//! Selection per device, on the first successful sysObjectID read: every
//! `default` profile applies, plus the vendor profile with the **longest
//! matching** `sys_object_id` prefix (its `extends` chain included). An
//! explicit `devices[].profile` pins that profile (plus chain) instead of
//! prefix matching. Configured `oids`/`walks`/`oid_group` always merge on
//! top. Naming/SYNTAX tables from *all* loaded profiles feed the shared
//! resolver at startup (fleet-wide, not per-device).

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// One parsed profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,

    /// Parent profiles applied together with this one.
    #[serde(default)]
    pub extends: Vec<String>,

    #[serde(default, rename = "match")]
    pub matcher: Matcher,

    /// Scalar OIDs (GET).
    #[serde(default)]
    pub oids: Vec<String>,

    /// Table columns / subtrees (WALK).
    #[serde(default)]
    pub walks: Vec<String>,

    /// OID → metric-name map (`{index}` placeholder for table columns).
    #[serde(default)]
    pub oid_names: HashMap<String, String>,

    /// OID → SMI SYNTAX hint (`Counter32`, `Gauge32`, `TimeTicks`, ...).
    #[serde(default)]
    pub oid_syntax: HashMap<String, String>,
}

/// How a profile is selected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Matcher {
    /// Applies to every device (base profiles).
    #[serde(default)]
    pub default: bool,

    /// sysObjectID prefixes this profile targets (vendor/model arcs).
    #[serde(default)]
    pub sys_object_id: Vec<String>,
}

/// The loaded profile set.
#[derive(Debug, Default)]
pub struct ProfileSet {
    profiles: HashMap<String, Profile>,
}

/// Shipped base profiles, embedded so a bare install has them.
const BUILTIN_PROFILES: [&str; 4] = [
    include_str!("../profiles/generic-device.toml"),
    include_str!("../profiles/network-interfaces.toml"),
    include_str!("../profiles/host-resources.toml"),
    include_str!("../profiles/entity-sensors.toml"),
];

impl ProfileSet {
    /// Load the embedded base profiles.
    pub fn builtin() -> Self {
        let mut set = Self::default();
        for toml in BUILTIN_PROFILES {
            let profile = parse_profile(toml).expect("embedded profile must parse");
            set.profiles.insert(profile.name.clone(), profile);
        }
        set
    }

    /// Load `.toml` profiles from a directory; same-name profiles override
    /// (user beats shipped). A malformed file is a hard error — a silently
    /// dropped profile means silently missing telemetry.
    pub fn load_dir(&mut self, dir: &std::path::Path) -> Result<usize> {
        let mut loaded = 0;
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("profile dir {} unreadable", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("profile {} unreadable", path.display()))?;
            let profile = parse_profile(&text)
                .with_context(|| format!("profile {} invalid", path.display()))?;
            self.profiles.insert(profile.name.clone(), profile);
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Validate cross-references (extends targets exist, no cycles).
    pub fn validate(&self) -> Result<()> {
        for profile in self.profiles.values() {
            self.chain(&profile.name)?;
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Combined `oid_names` of every loaded profile (fed to the shared
    /// resolver at startup).
    pub fn all_oid_names(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for p in self.profiles.values() {
            out.extend(p.oid_names.clone());
        }
        out
    }

    /// Combined `oid_syntax` of every loaded profile.
    pub fn all_oid_syntax(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for p in self.profiles.values() {
            out.extend(p.oid_syntax.clone());
        }
        out
    }

    /// A profile plus its transitive `extends` chain (parents first).
    /// Errors on unknown names and cycles.
    fn chain(&self, name: &str) -> Result<Vec<&Profile>> {
        let mut out: Vec<&Profile> = Vec::new();
        let mut visiting = Vec::new();
        self.chain_into(name, &mut out, &mut visiting)?;
        Ok(out)
    }

    fn chain_into<'a>(
        &'a self,
        name: &str,
        out: &mut Vec<&'a Profile>,
        visiting: &mut Vec<String>,
    ) -> Result<()> {
        if visiting.iter().any(|v| v == name) {
            bail!("profile extends cycle through {name:?}");
        }
        if out.iter().any(|p| p.name == name) {
            return Ok(());
        }
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| anyhow!("profile {name:?} not found (referenced via extends/pin)"))?;
        visiting.push(name.to_string());
        for parent in &profile.extends {
            self.chain_into(parent, out, visiting)?;
        }
        visiting.pop();
        out.push(profile);
        Ok(())
    }

    /// Select the profiles for a device: every `default` profile, plus —
    /// when `pinned` is unset — the vendor profile whose `sys_object_id`
    /// prefix matches `sys_object_id` longest (with its extends chain).
    /// A pinned profile replaces prefix matching but keeps the defaults.
    pub fn select(&self, sys_object_id: Option<&str>, pinned: Option<&str>) -> Result<Selection> {
        let mut ordered: Vec<&Profile> = Vec::new();

        // Defaults first (stable order by name for determinism).
        let mut defaults: Vec<&Profile> = self
            .profiles
            .values()
            .filter(|p| p.matcher.default)
            .collect();
        defaults.sort_by(|a, b| a.name.cmp(&b.name));
        for p in defaults {
            for member in self.chain(&p.name)? {
                if !ordered.iter().any(|o| o.name == member.name) {
                    ordered.push(member);
                }
            }
        }

        // Vendor selection.
        let vendor = if let Some(pin) = pinned {
            Some(pin.to_string())
        } else if let Some(oid) = sys_object_id {
            self.profiles
                .values()
                .filter(|p| !p.matcher.default)
                .filter_map(|p| {
                    p.matcher
                        .sys_object_id
                        .iter()
                        .filter(|prefix| oid.starts_with(prefix.as_str()))
                        .map(|prefix| (prefix.len(), p.name.clone()))
                        .max()
                })
                .max()
                .map(|(_, name)| name)
        } else {
            None
        };
        if let Some(name) = &vendor {
            for member in self.chain(name)? {
                if !ordered.iter().any(|o| o.name == member.name) {
                    ordered.push(member);
                }
            }
        }

        let mut oids = Vec::new();
        let mut walks = Vec::new();
        for p in &ordered {
            for oid in &p.oids {
                if !oids.contains(oid) {
                    oids.push(oid.clone());
                }
            }
            for walk in &p.walks {
                if !walks.contains(walk) {
                    walks.push(walk.clone());
                }
            }
        }

        Ok(Selection {
            applied: ordered.iter().map(|p| p.name.clone()).collect(),
            oids,
            walks,
        })
    }
}

/// The outcome of profile selection for one device.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Applied profile names, in application order.
    pub applied: Vec<String>,
    pub oids: Vec<String>,
    pub walks: Vec<String>,
}

fn parse_profile(text: &str) -> Result<Profile> {
    let profile: Profile = toml::from_str(text)?;
    if profile.name.is_empty() {
        bail!("profile has an empty name");
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_with(extra: &[&str]) -> ProfileSet {
        let mut set = ProfileSet::builtin();
        for toml in extra {
            let p = parse_profile(toml).unwrap();
            set.profiles.insert(p.name.clone(), p);
        }
        set.validate().unwrap();
        set
    }

    #[test]
    fn builtin_profiles_parse_and_validate() {
        let set = ProfileSet::builtin();
        set.validate().unwrap();
        assert!(set.get("generic-device").is_some());
        assert!(set.get("network-interfaces").is_some());
        assert!(set.get("host-resources").is_some());
        assert!(set.get("entity-sensors").is_some());
        // All shipped names are chunk-grammar-valid (lowercase).
        for name in set.all_oid_names().values() {
            for chunk in name.split('/') {
                let plain = chunk.replace("{index}", "1");
                assert!(
                    zenkey::grammar::is_valid_plain_chunk(&plain),
                    "shipped profile name {name:?} violates the chunk grammar"
                );
            }
        }
    }

    #[test]
    fn defaults_apply_without_sys_object_id() {
        let set = ProfileSet::builtin();
        let sel = set.select(None, None).unwrap();
        assert!(sel.applied.contains(&"generic-device".to_string()));
        assert!(sel.applied.contains(&"network-interfaces".to_string()));
        // host-resources / entity-sensors are opt-in, not default.
        assert!(!sel.applied.contains(&"host-resources".to_string()));
        assert!(!sel.oids.is_empty());
        assert!(!sel.walks.is_empty());
    }

    #[test]
    fn longest_sys_object_id_prefix_wins() {
        let set = set_with(&[
            r#"
name = "acme"
oids = ["1.3.6.1.4.1.4242.1.1.0"]
[match]
sys_object_id = ["1.3.6.1.4.1.4242."]
"#,
            r#"
name = "acme-switch"
extends = ["acme"]
walks = ["1.3.6.1.4.1.4242.1.2"]
[match]
sys_object_id = ["1.3.6.1.4.1.4242.1."]
"#,
        ]);
        let sel = set.select(Some("1.3.6.1.4.1.4242.1.99"), None).unwrap();
        // The more specific profile wins and pulls its parent in.
        assert!(sel.applied.contains(&"acme-switch".to_string()));
        assert!(sel.applied.contains(&"acme".to_string()));
        assert!(sel.oids.contains(&"1.3.6.1.4.1.4242.1.1.0".to_string()));
        assert!(sel.walks.contains(&"1.3.6.1.4.1.4242.1.2".to_string()));

        // A non-matching device gets neither.
        let sel = set.select(Some("1.3.6.1.4.1.9.1.1"), None).unwrap();
        assert!(!sel.applied.contains(&"acme".to_string()));
    }

    #[test]
    fn pin_overrides_matching() {
        let set = ProfileSet::builtin();
        let sel = set
            .select(Some("1.3.6.1.4.1.4242.1.99"), Some("host-resources"))
            .unwrap();
        assert!(sel.applied.contains(&"host-resources".to_string()));
    }

    #[test]
    fn unknown_pin_fails_loudly() {
        let set = ProfileSet::builtin();
        assert!(set.select(None, Some("nope")).is_err());
    }

    #[test]
    fn extends_cycle_is_detected() {
        let mut set = ProfileSet::default();
        for toml in [
            "name = \"a\"\nextends = [\"b\"]\n",
            "name = \"b\"\nextends = [\"a\"]\n",
        ] {
            let p = parse_profile(toml).unwrap();
            set.profiles.insert(p.name.clone(), p);
        }
        assert!(set.validate().is_err());
    }
}
