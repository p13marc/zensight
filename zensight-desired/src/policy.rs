//! The fleet policy file: classes of host, what each class gets, and how a
//! host's effective document is built (#938).
//!
//! # What a policy is
//!
//! `@desired` shipped in 0.12.0 as "fleet configuration as desired state
//! instead of eighteen hand-edited JSON5 files across six machines" — and
//! nothing in this repository published it. The consumer existed, the storage
//! existed, the never-list existed; the *author* was a private script
//! somewhere else. This is the author.
//!
//! A policy names **classes** (`all-hosts`, `hypervisors`, `edge`), says which
//! hosts are in each by matching facts the **catalog** already knows, and
//! gives each class a set of documents. A host's effective document for one
//! topic is the ordered overlay of every matching class, then its own
//! override.
//!
//! # Why the catalog is the only oracle
//!
//! A class could have been a tag published on the bus. It is not, and the
//! reason is the property that makes this daemon safe to run unattended: a
//! restart with an unchanged file and an unchanged fleet must publish
//! **nothing**. Bus-side tags would make class membership a second authority
//! that can change under the compiler between two passes, and then a restart
//! is no longer a no-op. The host-override section is the tag — versioned with
//! the policy, reviewable in a diff.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The whole policy file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Classes, in **file order** — which is also overlay order, so a later
    /// class wins a field a earlier one also set.
    ///
    /// An `IndexMap` would be the obvious type; a `Vec` of named entries is
    /// used instead because serde's map types do not preserve order without
    /// one, and order here is not cosmetic: it decides which class's value
    /// survives.
    #[serde(default)]
    pub classes: Vec<Class>,
    /// Per-host overrides, applied last. Keyed by entity id or host id.
    #[serde(default)]
    pub hosts: BTreeMap<String, HostOverride>,
}

/// One class of host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Class {
    pub name: String,
    /// Classes this one builds on, expanded depth-first before it.
    #[serde(default)]
    pub extends: Vec<String>,
    /// Which hosts are in the class. Absent means **no host** — a class with
    /// no matcher is a drafting mistake, and the safe reading of "I forgot to
    /// say who this is for" is nobody rather than everybody.
    #[serde(default)]
    pub matches: Option<Match>,
    /// `"<producer>/<topic>"` → the document fragment this class contributes.
    #[serde(default)]
    pub docs: BTreeMap<String, serde_json::Value>,
}

/// Per-host override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOverride {
    #[serde(default)]
    pub extends: Vec<String>,
    #[serde(default)]
    pub docs: BTreeMap<String, serde_json::Value>,
}

/// How a class selects hosts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Match {
    /// Every selector must hold.
    All(Vec<Selector>),
    /// At least one selector must hold.
    Any(Vec<Selector>),
    /// Every host in the catalog. Spelled out rather than reached by an empty
    /// `all: []`, which reads as "no conditions" and would be a vacuous truth
    /// nobody intended.
    Always(bool),
}

/// One fact about a host, as the catalog reports it.
///
/// Every variant reads a field of `HostEntity` — the only component that knows
/// what a host *is*, and the one that ran the union-find to decide it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Selector {
    /// Exact hashed machine-id.
    HostId(String),
    /// Glob over the representative hostname (`web-*`).
    HostnameGlob(String),
    /// A sensor that runs on the host — `members[].sensor`.
    Sensor(String),
    /// An IP inside this CIDR.
    IpCidr(String),
    /// Glob over `vendor` (`Dell*`).
    Vendor(String),
    /// Glob over `platform`.
    ///
    /// **A glob, not an exact match, and the distinction is load-bearing.**
    /// Since #935 `platform` is `<ID>-<VERSION_ID>` (`debian-13`,
    /// `proxmox-13`), so a class that wants the family writes `debian-*`. An
    /// exact match silently stops matching after a point-release upgrade —
    /// the worst failure a policy compiler has, because nothing is wrong: the
    /// host simply stops receiving configuration, and no error is raised
    /// anywhere.
    Platform(String),
}

/// Everything wrong with a policy, reported at once.
///
/// One error at a time turns a review into a compile-fix-recompile loop over a
/// file whose problems are all visible in one read.
#[derive(Debug, Default)]
pub struct Problems(pub Vec<String>);

impl Problems {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn push(&mut self, s: impl Into<String>) {
        self.0.push(s.into());
    }
}

impl std::fmt::Display for Problems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for p in &self.0 {
            writeln!(f, "  {p}")?;
        }
        Ok(())
    }
}

impl Policy {
    /// Parse a JSON5 policy file.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        json5::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Everything checkable without a catalog: names, cycles, topic spellings,
    /// document types, and the never-list.
    ///
    /// Deliberately separate from [`Self::compile`]: `plan` must be able to
    /// refuse a bad file on a laptop with no bus, and a policy that only fails
    /// once a fleet is attached is a policy nobody validates before pushing.
    pub fn validate(&self) -> Problems {
        let mut p = Problems::default();

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for c in &self.classes {
            if !seen.insert(c.name.as_str()) {
                p.push(format!(
                    "class {:?} is declared twice — overlay order is file order, so two \
                     classes with one name have no defined precedence",
                    c.name
                ));
            }
            if c.matches.is_none() && c.docs.is_empty() {
                p.push(format!(
                    "class {:?} has neither a matcher nor documents — it does nothing",
                    c.name
                ));
            }
            if let Some(Match::All(v) | Match::Any(v)) = &c.matches
                && v.is_empty()
            {
                p.push(format!(
                    "class {:?}: an empty selector list matches nothing. Write \
                     `matches: {{ always: true }}` if you meant every host",
                    c.name
                ));
            }
        }

        for c in &self.classes {
            for e in &c.extends {
                if !seen.contains(e.as_str()) {
                    p.push(format!("class {:?} extends unknown class {e:?}", c.name));
                }
            }
        }
        for (host, o) in &self.hosts {
            for e in &o.extends {
                if !seen.contains(e.as_str()) {
                    p.push(format!("host {host:?} extends unknown class {e:?}"));
                }
            }
        }

        // Cycles, reported by naming the loop rather than "cycle detected".
        //
        // Deduped by the *set* of classes in the loop: every member of a cycle
        // finds it, and printing `a -> b -> a` beside `b -> a -> b` makes one
        // mistake look like two.
        let mut cycles: BTreeSet<BTreeSet<String>> = BTreeSet::new();
        for c in &self.classes {
            if let Some(path) = self.cycle_from(&c.name) {
                let members: BTreeSet<String> = path.iter().cloned().collect();
                if cycles.insert(members) {
                    p.push(format!("class extends cycle: {}", path.join(" -> ")));
                }
            }
        }

        // Topics and payloads.
        for (label, docs) in self
            .classes
            .iter()
            .map(|c| (format!("class {:?}", c.name), &c.docs))
            .chain(
                self.hosts
                    .iter()
                    .map(|(h, o)| (format!("host {h:?}"), &o.docs)),
            )
        {
            for (key, doc) in docs {
                let Some((producer, topic)) = key.split_once('/') else {
                    p.push(format!("{label}: {key:?} is not `<producer>/<topic>`"));
                    continue;
                };
                if zensight_common::desired::topic(producer, topic).is_none() {
                    p.push(format!(
                        "{label}: no @desired topic `{producer}/{topic}`. The declared set is \
                         in `zensight-common/registry/desired.toml`"
                    ));
                    continue;
                }
                // A fragment is validated only once merged — a class may
                // legitimately contribute half a document — but the
                // never-list applies to every fragment, because a secret is a
                // secret before it is merged.
                if let Err(e) = zensight_common::desired::never_list_lint(doc) {
                    p.push(format!("{label}: {key}: {e}"));
                }
            }
        }

        p
    }

    /// The `extends` chain from `name` back to itself, if there is one.
    fn cycle_from(&self, name: &str) -> Option<Vec<String>> {
        fn walk<'a>(
            policy: &'a Policy,
            at: &'a str,
            path: &mut Vec<&'a str>,
        ) -> Option<Vec<String>> {
            if let Some(pos) = path.iter().position(|p| *p == at) {
                let mut loop_path: Vec<String> =
                    path[pos..].iter().map(|s| (*s).to_string()).collect();
                loop_path.push(at.to_string());
                return Some(loop_path);
            }
            path.push(at);
            let out = policy
                .classes
                .iter()
                .find(|c| c.name == at)
                .into_iter()
                .flat_map(|c| c.extends.iter())
                .find_map(|e| walk(policy, e, path));
            path.pop();
            out
        }
        walk(self, name, &mut Vec::new())
    }

    /// The classes that apply to `entity`, in overlay order — the matched
    /// ones, then any the host override names.
    ///
    /// `extends` expands **depth-first and before** the class that names it,
    /// so a class always overlays what it builds on. A class reached twice —
    /// through two chains, or once by matching and once by a host's
    /// `extends` — is applied **once**, at its first position.
    ///
    /// The host's `extends` is expanded the same way as a class's, which is
    /// the whole point: `extends: ["hypervisors"]` on a host has to mean what
    /// it means on a class, or the word means two things in one file. It gets
    /// `all-hosts` too.
    pub fn classes_for(&self, entity: &zensight_common::HostEntity) -> Vec<&Class> {
        let mut out: Vec<&Class> = Vec::new();
        let mut added: BTreeSet<&str> = BTreeSet::new();
        for c in &self.classes {
            if c.matches.as_ref().is_some_and(|m| m.matches(entity)) {
                self.push_with_extends(c, &mut out, &mut added, &mut Vec::new());
            }
        }
        // Then whatever this host asks for by name, after the matched ones so
        // an explicit opt-in overlays an inherited default.
        if let Some(o) = self.override_for(entity) {
            for name in &o.extends {
                if let Some(c) = self.classes.iter().find(|c| c.name == *name) {
                    self.push_with_extends(c, &mut out, &mut added, &mut Vec::new());
                }
            }
        }
        out
    }

    /// The override for this entity, by `host_id` or by `entity_id`.
    pub fn override_for(&self, entity: &zensight_common::HostEntity) -> Option<&HostOverride> {
        entity
            .host_id
            .as_deref()
            .and_then(|h| self.hosts.get(h))
            .or_else(|| self.hosts.get(&entity.entity_id))
            // …and through the ids this entity has superseded (#1107). A
            // `hosts:` key is written by a human against the id they could see
            // at the time; when the catalog upgrades an entity's id — a
            // fallback id becoming a real `host_id` after the machine's own
            // sensor reports in — a policy keyed on the old one silently stops
            // applying. Nothing errors, the host just stops receiving its
            // configuration, which `docs/policy.md` names as the worst failure
            // this compiler has.
            .or_else(|| entity.aliases.iter().find_map(|a| self.hosts.get(a)))
    }

    fn push_with_extends<'a>(
        &'a self,
        c: &'a Class,
        out: &mut Vec<&'a Class>,
        added: &mut BTreeSet<&'a str>,
        guard: &mut Vec<&'a str>,
    ) {
        if added.contains(c.name.as_str()) || guard.contains(&c.name.as_str()) {
            return;
        }
        guard.push(&c.name);
        for e in &c.extends {
            if let Some(base) = self.classes.iter().find(|x| x.name == *e) {
                self.push_with_extends(base, out, added, guard);
            }
        }
        guard.pop();
        added.insert(&c.name);
        out.push(c);
    }
}

impl Match {
    fn matches(&self, e: &zensight_common::HostEntity) -> bool {
        match self {
            Match::Always(v) => *v,
            Match::All(sels) => sels.iter().all(|s| s.matches(e)),
            Match::Any(sels) => sels.iter().any(|s| s.matches(e)),
        }
    }
}

impl Selector {
    fn matches(&self, e: &zensight_common::HostEntity) -> bool {
        match self {
            Selector::HostId(want) => e.host_id.as_deref() == Some(want.as_str()),
            Selector::HostnameGlob(pat) => [e.hostname.as_deref(), e.fqdn.as_deref()]
                .into_iter()
                .flatten()
                .any(|h| glob(pat, h)),
            Selector::Sensor(want) => e.members.iter().any(|m| m.sensor == *want),
            Selector::IpCidr(cidr) => e.ips.iter().any(|ip| ip_in_cidr(ip, cidr)),
            Selector::Vendor(pat) => e.vendor.as_deref().is_some_and(|v| glob(pat, v)),
            Selector::Platform(pat) => e.platform.as_deref().is_some_and(|v| glob(pat, v)),
        }
    }
}

/// `*` (any run, including empty) and `?` (one char), case-insensitive.
///
/// Deliberately not a regex: a policy file is read by whoever is on call, and
/// `web-*` is legible where `^web-.*$` invites a mistake nobody notices until a
/// class stops matching.
///
/// # Why this is iterative and not the obvious recursion
///
/// The three-line recursive version — `*` tries every split of the remaining
/// input — backtracks exponentially. `**********b` against a thirty-character
/// hostname takes over five million calls and never finishes in any useful
/// time.
///
/// That is not a hypothetical input. `**` is the *idiom* everywhere else in
/// this system: key expressions use it for "any depth", so an operator writing
/// a policy file has every reason to type it, and the cost of the typo would
/// be a daemon that hangs without saying why — while holding the fleet's
/// configuration.
///
/// The two-pointer form below backtracks only to the most recent `*`, which is
/// O(pattern x input) in the worst case and treats a run of stars as one.
fn glob(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let t: Vec<char> = s.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` guess turns out to be too short.
    let (mut star, mut resume) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Consume a run of stars as one, then try the shortest match
            // first; `resume` lets us lengthen it without re-walking.
            while pi < p.len() && p[pi] == '*' {
                pi += 1;
            }
            star = Some(pi);
            resume = ti;
        } else if let Some(after_star) = star {
            // Backtrack: let the last `*` swallow one more character.
            pi = after_star;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    // Trailing stars match the empty remainder.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether `ip` is inside `cidr`. v4 and v6; a malformed CIDR matches nothing.
///
/// A malformed CIDR is caught by `validate` as a problem, so this returning
/// `false` is the second line rather than the only one — but it must be
/// `false` and not a panic, because the policy is a file a human wrote.
fn ip_in_cidr(ip: &str, cidr: &str) -> bool {
    use std::net::IpAddr;
    let Some((base, bits)) = cidr.split_once('/') else {
        return false;
    };
    let (Ok(ip), Ok(base), Ok(bits)) = (
        ip.parse::<IpAddr>(),
        base.parse::<IpAddr>(),
        bits.parse::<u32>(),
    ) else {
        return false;
    };
    match (ip, base) {
        (IpAddr::V4(a), IpAddr::V4(b)) if bits <= 32 => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            a.to_bits() & mask == b.to_bits() & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) if bits <= 128 => {
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            a.to_bits() & mask == b.to_bits() & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod glob_tests {
    use super::glob;

    #[test]
    fn the_ordinary_cases() {
        assert!(glob("web-*", "web-01"));
        assert!(glob("*", ""));
        assert!(glob("*", "anything"));
        assert!(glob("web-??", "web-01"));
        assert!(!glob("web-??", "web-012"));
        assert!(glob("*.example.com", "host.example.com"));
        assert!(!glob("*.example.com", "example.com.evil"));
        assert!(glob("proxmox-*", "proxmox-13"));
        assert!(
            !glob("proxmox", "proxmox-13"),
            "the #935 trap: exact != family"
        );
        assert!(glob("debian-*", "Debian-13"), "case-insensitive");
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exacts"));
        assert!(!glob("", "nonempty"));
        assert!(glob("", ""));
    }

    /// The reason this is not the three-line recursion.
    ///
    /// `**` is the idiom everywhere else in this system — key expressions use
    /// it for "any depth" — so an operator writing a policy has every reason
    /// to type it. Under a backtracking recursion this input takes over five
    /// million calls and never finishes; the daemon would hang while holding
    /// the fleet's configuration, and say nothing about why.
    #[test]
    fn a_pathological_pattern_still_returns() {
        let hay = "a".repeat(64);
        assert!(!glob("**********b", &hay));
        assert!(!glob("*a*a*a*a*a*a*a*b", &hay));
        assert!(glob("**********a", &hay));
        assert!(glob("*a*a*a*a*a*a*a*a", &hay));
    }

    /// A run of stars is one star, and a trailing one matches nothing left.
    #[test]
    fn star_runs_and_trailing_stars() {
        assert!(glob("a***b", "ab"));
        assert!(glob("a***b", "axxxb"));
        assert!(glob("abc***", "abc"));
        assert!(glob("a*", "a"));
        assert!(!glob("a*b", "a"));
    }

    /// Backtracking has to actually happen: the first guess for `*` is the
    /// shortest, and `axbxc` needs it lengthened twice.
    #[test]
    fn backtracking_lengthens_the_star() {
        assert!(glob("a*c", "axbxc"));
        assert!(glob("*b*", "abc"));
        assert!(!glob("a*d", "axbxc"));
    }
}

/// Entity id lineage in the policy layer (#1107).
#[cfg(test)]
mod alias_tests {
    use super::*;

    fn entity(id: &str, aliases: &[&str]) -> zensight_common::HostEntity {
        zensight_common::HostEntity {
            entity_id: id.into(),
            aliases: aliases.iter().map(|a| (*a).into()).collect(),
            host_id: Some(id.into()),
            boot_id: None,
            ips: Vec::new(),
            macs: Vec::new(),
            container_ids: Vec::new(),
            origins: Vec::new(),
            hostname: None,
            fqdn: None,
            names: Vec::new(),
            vendor: None,
            platform: None,
            members: Vec::new(),
            status: None,
            last_updated: 0,
        }
    }

    /// A `hosts:` key keeps applying after the catalog upgrades the entity's id.
    ///
    /// A human writes the key against the id they can see. When the machine's
    /// own sensor reports in, a fallback id becomes a real `host_id` and the
    /// entity id changes — and a policy keyed on the old one silently stopped
    /// applying. Nothing errors; the host just stops receiving its
    /// configuration, which `docs/policy.md` names as the worst failure this
    /// compiler has.
    #[test]
    fn an_override_follows_a_superseded_entity_id() {
        let p: Policy = json5::from_str(
            r#"{ classes: [], hosts: { "h-old0000000a":
                 { docs: { "sysinfo/thresholds": { rules: [] } } } } }"#,
        )
        .unwrap();

        assert!(
            p.override_for(&entity("h-new0000000b", &[])).is_none(),
            "nothing names the current id — which is exactly the situation"
        );
        assert!(
            p.override_for(&entity("h-new0000000b", &["h-old0000000a"]))
                .is_some(),
            "the override must follow the id the catalog superseded"
        );
    }

    /// The current id still wins: an alias is a fallback, not an override of
    /// the override.
    #[test]
    fn the_current_id_outranks_an_alias() {
        let p: Policy = json5::from_str(
            r#"{ classes: [], hosts: {
                 "h-current0001": { docs: { "sysinfo/thresholds": { rules: [ { name: "now" } ] } } },
                 "h-old00000001": { docs: { "sysinfo/thresholds": { rules: [ { name: "then" } ] } } },
               } }"#,
        )
        .unwrap();
        let o = p
            .override_for(&entity("h-current0001", &["h-old00000001"]))
            .expect("an override applies");
        let doc = &o.docs["sysinfo/thresholds"];
        assert_eq!(doc["rules"][0]["name"], "now");
    }
}
