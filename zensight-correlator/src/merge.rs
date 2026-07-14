//! Deterministic evidence merge — the correlation core.
//!
//! Pure module: NO zenoh, NO tokio, NO clock. Input is a snapshot of
//! (TTL-filtered) [`HostEvidence`] plus the merge kill-switches; output is a
//! `Vec<HostEntity>`. It is a **pure function of the evidence set** — the same
//! evidence in any input order produces byte-identical entities (ids, member
//! order, ip/mac order) — which is what makes the correlator stateless and
//! restart-recoverable. Determinism is pinned by tests here and in commit 5.
//!
//! # Algorithm
//!
//! Each [`HostEvidence`] is one **node**. A union-find (disjoint-set) over node
//! indices groups nodes that ranked rules say are the same host. Rules generate
//! candidate *bridges* `(a, b, rule, confidence)`; bridges are applied
//! strongest-first, subject to the host_id-conflict guard.
//!
//! ## Rule table (a bridge links two evidence nodes)
//!
//! | rule             | condition                                                 | base confidence |
//! |------------------|-----------------------------------------------------------|-----------------|
//! | `host_id`        | both have `host_id` AND equal                             | 1.0             |
//! | `cloud_instance` | both have `cloud` AND equal `(provider, instance_id)`     | 0.95            |
//! | `mac_ip`         | share ≥1 MAC **AND** share ≥1 IP                          | 0.8             |
//! | `fqdn`           | both have non-empty `fqdn` AND equal (case-insensitive)   | 0.5             |
//! | `hostname`       | both have `hostname` AND equal (case-insensitive), if enabled | 0.25        |
//!
//! - **`cloud_instance` is authoritative per provider** (#311): a cloud control
//!   plane never hands out the same instance id twice, so it merges almost as
//!   strongly as `host_id` — and still saves the day when a cloned image left
//!   evidence without a machine-id. `container_id` is deliberately **not** a
//!   rule: container ids are only unique per host runtime, never across hosts.
//!
//! - **IP alone is never a bridge** (DHCP/NAT reuse) — it is a hint only.
//! - **MAC alone is never a bridge** (VMs clone MACs) — only MAC+IP together.
//! - **observer down-weight:** if *either* endpoint is a third-party claim
//!   (`observer.is_some()`), the bridge confidence is multiplied by 0.8.
//! - **host_id-conflict guard:** two nodes with *different* host_ids must never
//!   land in one set, regardless of any weaker rule. A host_id is aggregated per
//!   union-find set; any bridge that would place two distinct host_ids in one
//!   set is dropped (see [`UnionFind::try_union`]).
//!
//! Bridges are sorted by a **content-derived** key (never by input index) so the
//! applied order — and therefore the resulting partition — does not depend on
//! input order.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use zensight_common::{AssertionKind, HostEntity, HostEvidence, MemberClaim, OperatorAssertion};

use crate::config::RulesConfig;

/// A candidate bridge between two evidence nodes.
struct Bridge {
    a: usize,
    b: usize,
    rule: Rule,
    /// Post-downweight confidence.
    confidence: f32,
}

/// The five merge rules, ordered strongest-first by their discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    Hostname = 0,
    Fqdn = 1,
    MacIp = 2,
    CloudInstanceId = 3,
    HostId = 4,
}

impl Rule {
    fn base_confidence(self) -> f32 {
        match self {
            Rule::HostId => 1.0,
            Rule::CloudInstanceId => 0.95,
            Rule::MacIp => 0.8,
            Rule::Fqdn => 0.5,
            Rule::Hostname => 0.25,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Rule::HostId => "host_id",
            Rule::CloudInstanceId => "cloud_instance",
            Rule::MacIp => "mac_ip",
            Rule::Fqdn => "fqdn",
            Rule::Hostname => "hostname",
        }
    }
}

/// The operator's assertions, indexed for the merge (#473, RFC 06 §5.4).
///
/// Built from the `@catalog/state/assertion/*` documents — which is to say, from
/// the bus. The merge stays a pure function; this is just another input to it.
#[derive(Debug, Default, Clone)]
pub struct Assertions {
    /// `old origin → new origin`, from every `link`. Applied as a **rewrite of
    /// the host_id before correlation**, not as a special bridge — see
    /// [`Assertions::canon`].
    links: HashMap<String, String>,
    /// Unordered origin pairs an operator has vetoed. Both directions are
    /// inserted so lookup needs no ordering convention.
    forbidden: HashSet<(String, String)>,
}

impl Assertions {
    /// Index a set of assertion documents. `unlink` wins over `link` for the
    /// same pair regardless of arrival order: a veto is the operator's *latest*
    /// word by construction (issuing one is how you retire a link), and letting
    /// map order decide would make the outcome depend on HashMap iteration.
    pub fn new(docs: impl IntoIterator<Item = OperatorAssertion>) -> Self {
        let mut links = HashMap::new();
        let mut forbidden: HashSet<(String, String)> = HashSet::new();
        let docs: Vec<_> = docs.into_iter().collect();

        for d in &docs {
            if d.kind == AssertionKind::Unlink {
                forbidden.insert((d.old.clone(), d.new.clone()));
                forbidden.insert((d.new.clone(), d.old.clone()));
            }
        }
        for d in &docs {
            if d.kind == AssertionKind::Link && !forbidden.contains(&(d.old.clone(), d.new.clone()))
            {
                links.insert(d.old.clone(), d.new.clone());
            }
        }

        let mut me = Self { links, forbidden };

        // A veto can still be violated *transitively*: `link a→c` and `link b→c`
        // canonicalize a and b to the same id, merging the very pair an operator
        // said were different machines. Direct-pair filtering above does not see
        // this. Break the chain by dropping the link out of the vetoed id — the
        // veto is the operator's most recent word, so it wins and the id is left
        // standing alone. Bounded: each pass removes at least one edge.
        for _ in 0..16 {
            let violation = me
                .forbidden
                .iter()
                .find(|(a, b)| me.canon(a) == me.canon(b))
                .map(|(a, b)| (a.clone(), b.clone()));
            let Some((a, b)) = violation else { break };
            tracing::warn!(
                a = %a, b = %b,
                "operator vetoed this pair, but a link chain still merges them — \
                 dropping the link so the veto holds"
            );
            if me.links.remove(&a).is_none() {
                me.links.remove(&b);
            }
        }
        me
    }

    /// Follow the link chain to the origin an id should now be known by.
    ///
    /// **This is the whole of `link`'s implementation.** A reinstall gives one
    /// machine two origins; canonicalizing the old to the new makes the existing
    /// `host_id` rule bucket their evidence together, makes the conflict guard
    /// see one id instead of two, and makes `entity_id_for` return the new
    /// origin — with no forced bridge, no guard bypass, and no second code path
    /// through the merge. A chain (reinstalled twice) collapses in one pass.
    ///
    /// Bounded against a cycle an operator could type in (`link a b` +
    /// `link b a`): a cycle resolves to *some* member of it, deterministically,
    /// rather than hanging the correlator.
    fn canon(&self, id: &str) -> String {
        let mut cur = id;
        for _ in 0..16 {
            match self.links.get(cur) {
                Some(next) if next != cur => cur = next.as_str(),
                _ => break,
            }
        }
        cur.to_string()
    }

    /// The origins that were linked *away* into `entity_id` — the ids a consumer
    /// may still be holding, and therefore the ones that need alias records.
    fn aliases_of(&self, entity_id: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .links
            .keys()
            .filter(|old| self.canon(old) == entity_id && old.as_str() != entity_id)
            .cloned()
            .collect();
        v.sort();
        v
    }
}

/// Union-find with a host_id aggregated per set for the conflict guard.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
    /// Aggregated host_id for the set rooted at each index (only meaningful at a
    /// root). A set holds at most one host_id; the union that would introduce a
    /// second is the one the guard blocks.
    set_host_id: Vec<Option<String>>,
}

impl UnionFind {
    fn new(node_host_ids: &[Option<String>]) -> Self {
        let n = node_host_ids.len();
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
            set_host_id: node_host_ids.to_vec(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // path halving
            x = self.parent[x];
        }
        x
    }

    /// Attempt to union the sets containing `a` and `b`. Returns `false` (and
    /// unions nothing) if it would merge two distinct host_ids — the conflict
    /// guard. Root choice (by size) does not affect the final partition or any
    /// entity field, which are computed order-independently.
    fn try_union(&mut self, a: usize, b: usize) -> bool {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return true; // already together — nothing to do, not a conflict
        }
        // host_id-conflict guard: never merge two distinct host_ids.
        //
        // An operator veto (`unlink`, #473) needs no arm here, and adding one
        // would be dead code: by the time a bridge is applied, host_ids have been
        // canonicalized (`Assertions::canon`), so a vetoed pair is either two
        // distinct ids — which this arm already refuses — or one id, which is
        // precisely what the veto prevents from happening. The veto is therefore
        // enforced where it is decidable: in `Assertions::new`, on the link map.
        match (&self.set_host_id[ra], &self.set_host_id[rb]) {
            (Some(ha), Some(hb)) if ha != hb => return false,
            _ => {}
        }
        let merged_host_id = self.set_host_id[ra]
            .clone()
            .or_else(|| self.set_host_id[rb].clone());
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
        self.set_host_id[big] = merged_host_id;
        true
    }
}

/// Normalize a MAC for bucketing/identity: lowercased.
fn norm_mac(mac: &str) -> String {
    mac.to_ascii_lowercase()
}

/// Whether a MAC is empty or all-zero (`00:00:...`) — not an identifying claim.
fn is_zero_mac(mac: &str) -> bool {
    mac.is_empty() || mac.chars().all(|c| matches!(c, '0' | ':' | '-'))
}

/// The 12-hex-char entity-id suffix from a sha256 of `input`.
fn sha256_12(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let hex = format!("{:x}", digest);
    hex[..12].to_string()
}

/// Stable, input-order-independent key for a node: its `(sensor, source)` — the
/// evidence keyspace guarantees this is unique per node.
fn node_key(ev: &HostEvidence) -> String {
    format!("{}\u{1f}{}", ev.sensor, ev.source)
}

/// Merge the evidence snapshot into entities. `names`/`status`/`aliases` are
/// left for the engine to fill; this produces the identifying + membership core.
pub fn correlate(
    evidence: &[HostEvidence],
    rules: &RulesConfig,
    assertions: &Assertions,
) -> Vec<HostEntity> {
    if evidence.is_empty() {
        return Vec::new();
    }

    // Operator links (#473) are applied *here*, as a rewrite of each node's
    // host_id — before a single bridge is generated. Everything downstream (the
    // host_id bucket rule, the conflict guard, `entity_id_for`) then does the
    // right thing without knowing assertions exist.
    let node_host_ids: Vec<Option<String>> = evidence
        .iter()
        .map(|e| e.host_id.as_deref().map(|h| assertions.canon(h)))
        .collect();
    let mut uf = UnionFind::new(&node_host_ids);

    let bridges = generate_bridges(evidence, rules, &node_host_ids);

    // Per-node strongest applied bridge: (rule, confidence). None ⇒ singleton
    // seed ("self").
    let mut node_rule: Vec<Option<(Rule, f32)>> = vec![None; evidence.len()];

    for br in &bridges {
        if uf.try_union(br.a, br.b) {
            credit(&mut node_rule[br.a], br.rule, br.confidence);
            credit(&mut node_rule[br.b], br.rule, br.confidence);
        } else {
            tracing::debug!(
                a = %node_key(&evidence[br.a]),
                b = %node_key(&evidence[br.b]),
                rule = br.rule.as_str(),
                "dropped bridge — would merge two distinct host_ids (conflict guard)"
            );
        }
    }

    let mut entities = build_entities(evidence, &mut uf, &node_rule, &node_host_ids);

    // An id an operator linked away is an id consumers may still hold. Recording
    // it in `aliases` is what makes the publisher emit the `alias/<old>` record
    // (RFC 06 §5.4) — without this the merge is invisible to anyone holding the
    // old id, which is the same as not having merged.
    for e in &mut entities {
        for old in assertions.aliases_of(&e.entity_id) {
            if !e.aliases.contains(&old) {
                e.aliases.push(old);
            }
        }
        e.aliases.sort();
    }
    entities
}

/// Update a node's strongest-rule credit. "Strongest" is by rule priority first
/// (host_id > mac_ip > fqdn > hostname), then by confidence — so a full-strength
/// bridge beats a downweighted one of the same rule.
fn credit(slot: &mut Option<(Rule, f32)>, rule: Rule, confidence: f32) {
    let better = match slot {
        None => true,
        Some((r, c)) => (rule, confidence) > (*r, *c),
    };
    if better {
        *slot = Some((rule, confidence));
    }
}

/// Generate all candidate bridges, then sort them into a deterministic,
/// input-order-independent application order.
fn generate_bridges(
    evidence: &[HostEvidence],
    rules: &RulesConfig,
    node_host_ids: &[Option<String>],
) -> Vec<Bridge> {
    let mut bridges: Vec<Bridge> = Vec::new();

    // host_id: exact-equal buckets (each bucket is one certain host).
    //
    // Buckets on the *canonical* id, not the raw payload one — that single
    // substitution is what makes an operator `link` merge a reinstall: the old
    // and new origins land in one bucket and the ordinary host_id rule does the
    // rest, at full confidence, with no bridge type of its own.
    if rules.host_id {
        let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, h) in node_host_ids.iter().enumerate() {
            if let Some(h) = h.as_deref() {
                buckets.entry(h).or_default().push(i);
            }
        }
        emit_bucket_bridges(&buckets, Rule::HostId, evidence, &mut bridges);
    }

    // cloud_instance: exact-equal `(provider, instance_id)` buckets (#311) —
    // authoritative per provider, so it sits just below host_id.
    if rules.cloud_instance {
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in evidence.iter().enumerate() {
            if let Some(cloud) = &e.cloud
                && !cloud.instance_id.is_empty()
            {
                buckets
                    .entry(format!("{}\u{1f}{}", cloud.provider, cloud.instance_id))
                    .or_default()
                    .push(i);
            }
        }
        emit_bucket_bridges(&buckets, Rule::CloudInstanceId, evidence, &mut bridges);
    }

    // mac_ip: bucket by MAC, then pair within a bucket only if IPs also overlap.
    if rules.mac_ip {
        let mut mac_buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in evidence.iter().enumerate() {
            for mac in &e.macs {
                if is_zero_mac(mac) {
                    continue;
                }
                mac_buckets.entry(norm_mac(mac)).or_default().push(i);
            }
        }
        for nodes in mac_buckets.values() {
            for x in 0..nodes.len() {
                for y in (x + 1)..nodes.len() {
                    let (a, b) = (nodes[x], nodes[y]);
                    if a == b {
                        continue;
                    }
                    if ips_overlap(&evidence[a].ips, &evidence[b].ips) {
                        push_bridge(evidence, a, b, Rule::MacIp, &mut bridges);
                    }
                }
            }
        }
    }

    // fqdn: case-insensitive equal buckets.
    if rules.fqdn {
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in evidence.iter().enumerate() {
            if let Some(f) = e.fqdn.as_deref()
                && !f.is_empty()
            {
                buckets.entry(f.to_ascii_lowercase()).or_default().push(i);
            }
        }
        emit_bucket_bridges(&buckets, Rule::Fqdn, evidence, &mut bridges);
    }

    // hostname: case-insensitive equal buckets (weakest; kill-switchable).
    if rules.hostname_enabled {
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in evidence.iter().enumerate() {
            if let Some(h) = e.hostname.as_deref()
                && !h.is_empty()
            {
                buckets.entry(h.to_ascii_lowercase()).or_default().push(i);
            }
        }
        emit_bucket_bridges(&buckets, Rule::Hostname, evidence, &mut bridges);
    }

    // Deterministic application order: strongest rule first, then higher
    // confidence, then by the two nodes' content keys (never their indices) so
    // the order is identical across shuffled input.
    bridges.sort_by(|x, y| {
        (y.rule as u8)
            .cmp(&(x.rule as u8))
            .then_with(|| {
                y.confidence
                    .partial_cmp(&x.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| bridge_key(x, evidence).cmp(&bridge_key(y, evidence)))
    });
    bridges
}

/// Content-derived, order-independent sort key for a bridge: its two node keys
/// sorted so `(a,b)` and `(b,a)` compare equal.
fn bridge_key(br: &Bridge, evidence: &[HostEvidence]) -> (String, String) {
    let ka = node_key(&evidence[br.a]);
    let kb = node_key(&evidence[br.b]);
    if ka <= kb { (ka, kb) } else { (kb, ka) }
}

/// Emit "spanning" bridges within each exact-equal bucket: every non-first node
/// bridged to the first. Because all nodes in the bucket share the rule's key
/// exactly, any spanning set yields the same union — so the arbitrary "to first"
/// choice does not affect the partition.
fn emit_bucket_bridges<K>(
    buckets: &HashMap<K, Vec<usize>>,
    rule: Rule,
    evidence: &[HostEvidence],
    out: &mut Vec<Bridge>,
) {
    for nodes in buckets.values() {
        if nodes.len() < 2 {
            continue;
        }
        let first = nodes[0];
        for &other in &nodes[1..] {
            push_bridge(evidence, first, other, rule, out);
        }
    }
}

/// Push a bridge, applying the observer down-weight if either endpoint is a
/// third-party claim.
fn push_bridge(evidence: &[HostEvidence], a: usize, b: usize, rule: Rule, out: &mut Vec<Bridge>) {
    let mut confidence = rule.base_confidence();
    if evidence[a].observer.is_some() || evidence[b].observer.is_some() {
        confidence *= 0.8;
    }
    out.push(Bridge {
        a,
        b,
        rule,
        confidence,
    });
}

/// Whether two IP lists share at least one address.
fn ips_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|ip| b.contains(ip))
}

/// Build one [`HostEntity`] per union-find set.
fn build_entities(
    evidence: &[HostEvidence],
    uf: &mut UnionFind,
    node_rule: &[Option<(Rule, f32)>],
    node_host_ids: &[Option<String>],
) -> Vec<HostEntity> {
    // Group node indices by their set root.
    let mut sets: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..evidence.len() {
        let root = uf.find(i);
        sets.entry(root).or_default().push(i);
    }

    let mut entities: Vec<HostEntity> = sets
        .into_values()
        .map(|members| build_entity(evidence, &members, node_rule, node_host_ids))
        .collect();

    // Sort entities by id for a stable output ordering.
    entities.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    entities
}

fn build_entity(
    evidence: &[HostEvidence],
    members: &[usize],
    node_rule: &[Option<(Rule, f32)>],
    node_host_ids: &[Option<String>],
) -> HostEntity {
    // The *canonical* host_id (post-`link`), not the raw payload one: after a
    // reinstall link, the entity must be known by the new origin.
    let host_id = members
        .iter()
        .filter_map(|&i| node_host_ids[i].clone())
        .next();

    let boot_id = members
        .iter()
        .filter_map(|&i| evidence[i].boot_id.clone())
        .min();

    let mut ips: Vec<String> = members
        .iter()
        .flat_map(|&i| evidence[i].ips.iter().cloned())
        .collect();
    ips.sort();
    ips.dedup();

    let mut macs: Vec<String> = members
        .iter()
        .flat_map(|&i| evidence[i].macs.iter().map(|m| norm_mac(m)))
        .filter(|m| !is_zero_mac(m))
        .collect();
    macs.sort();
    macs.dedup();

    // Containers the host's sensors run in (#311) — a descriptive union, never
    // a merge key (container ids are only unique per host runtime).
    let mut container_ids: Vec<String> = members
        .iter()
        .filter_map(|&i| evidence[i].container_id.clone())
        .collect();
    container_ids.sort();
    container_ids.dedup();

    let hostname = representative(members.iter().map(|&i| {
        (
            evidence[i].observer.is_none(),
            evidence[i].hostname.as_deref(),
        )
    }));
    let fqdn = representative(
        members
            .iter()
            .map(|&i| (evidence[i].observer.is_none(), evidence[i].fqdn.as_deref())),
    );
    let vendor = representative(members.iter().map(|&i| {
        (
            evidence[i].observer.is_none(),
            evidence[i].vendor.as_deref(),
        )
    }));
    let platform = representative(members.iter().map(|&i| {
        (
            evidence[i].observer.is_none(),
            evidence[i].platform.as_deref(),
        )
    }));

    let mut member_claims: Vec<MemberClaim> = members
        .iter()
        .map(|&i| {
            let (rule, confidence) = match node_rule[i] {
                Some((r, c)) => (r.as_str().to_string(), c),
                None => ("self".to_string(), 1.0),
            };
            MemberClaim {
                sensor: evidence[i].sensor.clone(),
                source: evidence[i].source.clone(),
                rule,
                confidence,
                last_seen: evidence[i].last_updated,
            }
        })
        .collect();
    member_claims.sort_by(|a, b| {
        a.sensor
            .cmp(&b.sensor)
            .then_with(|| a.source.cmp(&b.source))
    });

    let last_updated = members
        .iter()
        .map(|&i| evidence[i].last_updated)
        .max()
        .unwrap_or(0);

    let entity_id = entity_id_for(&host_id, &fqdn, &macs, &hostname, &ips, evidence, members);

    let mut entity = HostEntity {
        entity_id,
        aliases: Vec::new(),
        host_id,
        boot_id,
        ips,
        macs,
        container_ids,
        hostname,
        fqdn,
        names: Vec::new(),
        vendor,
        platform,
        members: member_claims,
        status: None,
        last_updated,
    };
    entity.canonicalize();
    entity
}

/// Pick a representative descriptive value, preferring a self-report
/// (`observer == None`) over a third-party claim; among equals, the
/// lexicographically-smallest value (for determinism).
fn representative<'a>(candidates: impl Iterator<Item = (bool, Option<&'a str>)>) -> Option<String> {
    candidates
        .filter_map(|(is_self, v)| v.filter(|s| !s.is_empty()).map(|s| (is_self, s)))
        // key: self-reports first (!is_self == false sorts before true), then min value.
        .min_by(|(sa, va), (sb, vb)| (!sa, *va).cmp(&(!sb, *vb)))
        .map(|(_, v)| v.to_string())
}

/// Compute the stable entity id per §9.3.
///
/// # Tie-break (order-independent by construction)
///
/// 1. If any member has a `host_id` (the conflict guard guarantees at most one
///    distinct value per set), the id IS that host_id — the v1 origin id
///    (`h-<12hex>`, RFC 06 §2).
///    `host_id` is already a sha256 hex, so this is just its 12-char prefix.
/// 2. Otherwise the id is `h-<first 12 hex of sha256(best_key)>`, where
///    `best_key` is chosen by taking the **highest-priority category any member
///    has** — `fqdn` > `mac` > `hostname` > `ip` — and, within that category,
///    the **lexicographically-smallest** value across the whole set. Picking a
///    whole category before comparing values makes the choice independent of
///    which member happened to be listed first.
/// 3. If a set has none of those (no fqdn/mac/hostname/ip anywhere), the id
///    falls back to `sha256` of the lexicographically-smallest member
///    `sensor\u{1f}source` key — every node always has one, so an id always
///    exists.
///
/// `fqdn`/`hostname` are lowercased before hashing (they merge case-insensitively),
/// so casing differences can't split an id.
fn entity_id_for(
    host_id: &Option<String>,
    fqdn: &Option<String>,
    macs: &[String],
    hostname: &Option<String>,
    ips: &[String],
    evidence: &[HostEvidence],
    members: &[usize],
) -> String {
    if let Some(h) = host_id {
        // v1 (RFC 06 §2): the payload host_id IS the origin id (`h-<12hex>`)
        // — entity id ≡ origin id, no re-derivation. Legacy full-hash values
        // (pre-#453 sensors) still truncate.
        if h.starts_with("h-") && h.len() == 14 {
            return h.clone();
        }
        let prefix: String = h.chars().take(12).collect();
        return format!("h-{}", prefix);
    }

    // Highest-priority category present anywhere in the set → min value within it.
    let best_key: String = if let Some(f) = fqdn {
        f.to_ascii_lowercase()
    } else if let Some(m) = macs.iter().min() {
        m.clone()
    } else if let Some(h) = hostname {
        h.to_ascii_lowercase()
    } else if let Some(ip) = ips.iter().min() {
        ip.clone()
    } else {
        members
            .iter()
            .map(|&i| node_key(&evidence[i]))
            .min()
            .unwrap_or_default()
    };

    format!("h-{}", sha256_12(&best_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(sensor: &str, source: &str) -> HostEvidence {
        HostEvidence {
            sensor: sensor.into(),
            source: source.into(),
            observer: None,
            host_id: None,
            boot_id: None,
            hostname: None,
            fqdn: None,
            ips: vec![],
            macs: vec![],
            vendor: None,
            platform: None,
            container_id: None,
            cloud: None,
            last_updated: 1000,
        }
    }

    fn hid(byte: u8) -> String {
        // 64-hex-char sha256-shaped host_id.
        format!("{:02x}", byte).repeat(32)
    }

    fn cloud(provider: &str, instance_id: &str) -> zensight_common::CloudFacts {
        zensight_common::CloudFacts {
            provider: provider.into(),
            instance_id: instance_id.into(),
            region: None,
            account: None,
        }
    }

    #[test]
    fn host_id_rule_bridges_two_self_reports() {
        let mut a = ev("sysinfo", "host1");
        a.host_id = Some(hid(0xab));
        let mut b = ev("netlink", "host1");
        b.host_id = Some(hid(0xab));
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].members.len(), 2);
        assert!(ents[0].members.iter().all(|m| m.rule == "host_id"));
        assert!((ents[0].members[0].confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cloud_instance_rule_bridges() {
        // Same (provider, instance_id) merges even without any machine-id —
        // the cloned-image case: evidence lacking host_id still binds.
        let mut a = ev("sysinfo", "vm1");
        a.cloud = Some(cloud("aws", "i-0abc"));
        let mut b = ev("netlink", "vm1");
        b.cloud = Some(cloud("aws", "i-0abc"));
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        assert!(ents[0].members.iter().all(|m| m.rule == "cloud_instance"));
        assert!((ents[0].members[0].confidence - 0.95).abs() < 1e-5);
    }

    #[test]
    fn cloud_instance_requires_same_provider() {
        // Instance ids are provider-scoped: "12345" on aws ≠ "12345" on gcp.
        let mut a = ev("sysinfo", "a");
        a.cloud = Some(cloud("aws", "12345"));
        let mut b = ev("sysinfo", "b");
        b.cloud = Some(cloud("gcp", "12345"));
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(
            ents.len(),
            2,
            "same instance id on different providers must NOT merge"
        );
    }

    #[test]
    fn cloud_instance_rule_disabled_makes_no_bridge() {
        let mut a = ev("sysinfo", "a");
        a.cloud = Some(cloud("aws", "i-0abc"));
        let mut b = ev("netlink", "b");
        b.cloud = Some(cloud("aws", "i-0abc"));
        let rules = RulesConfig {
            cloud_instance: false,
            ..RulesConfig::default()
        };
        let ents = correlate(&[a, b], &rules, &Assertions::default());
        assert_eq!(ents.len(), 2);
    }

    #[test]
    fn host_id_conflict_guard_blocks_cloud_bridge() {
        // Distinct host_ids stay separate even when a (mis)configured pair
        // reports the same cloud instance — host_id remains the top authority.
        let mut a = ev("sysinfo", "a");
        a.host_id = Some(hid(0x11));
        a.cloud = Some(cloud("aws", "i-0abc"));
        let mut b = ev("sysinfo", "b");
        b.host_id = Some(hid(0x22));
        b.cloud = Some(cloud("aws", "i-0abc"));
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 2);
    }

    #[test]
    fn container_ids_unioned_onto_entity_but_never_merge() {
        // Two hosts whose sensors run in the same-named container must NOT
        // merge on it (container ids are host-scoped) ...
        let mut a = ev("sysinfo", "a");
        a.container_id = Some("c".repeat(64));
        let mut b = ev("sysinfo", "b");
        b.container_id = Some("c".repeat(64));
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 2, "container_id must never be a merge key");

        // ... but within one host, members' container ids union onto the entity.
        let mut x = ev("sysinfo", "h");
        x.host_id = Some(hid(0x55));
        x.container_id = Some("a".repeat(64));
        let mut y = ev("netlink", "h");
        y.host_id = Some(hid(0x55));
        y.container_id = Some("b".repeat(64));
        let mut z = ev("netring", "h");
        z.host_id = Some(hid(0x55));
        z.container_id = Some("a".repeat(64)); // duplicate → deduped
        let ents = correlate(&[x, y, z], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].container_ids, vec!["a".repeat(64), "b".repeat(64)]);
    }

    #[test]
    fn mac_ip_rule_bridges() {
        let mut a = ev("netlink", "a");
        a.macs = vec!["AA:BB:CC:DD:EE:FF".into()];
        a.ips = vec!["10.0.0.1".into()];
        let mut b = ev("netring", "b");
        b.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        b.ips = vec!["10.0.0.1".into()];
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1, "shared MAC+IP must merge");
        assert!(ents[0].members.iter().any(|m| m.rule == "mac_ip"));
    }

    #[test]
    fn ip_alone_is_not_a_bridge() {
        let mut a = ev("netlink", "a");
        a.ips = vec!["10.0.0.1".into()];
        let mut b = ev("netring", "b");
        b.ips = vec!["10.0.0.1".into()];
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 2, "shared IP alone (DHCP reuse) must NOT merge");
    }

    #[test]
    fn mac_alone_is_not_a_bridge() {
        let mut a = ev("netlink", "a");
        a.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        a.ips = vec!["10.0.0.1".into()];
        let mut b = ev("netring", "b");
        b.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        b.ips = vec!["10.0.0.2".into()]; // different IP
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 2, "shared MAC alone (VM clone) must NOT merge");
    }

    #[test]
    fn fqdn_rule_bridges_case_insensitive() {
        let mut a = ev("s", "a");
        a.fqdn = Some("Host1.Example.COM".into());
        let mut b = ev("s", "b");
        b.fqdn = Some("host1.example.com".into());
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        assert!(ents[0].members.iter().any(|m| m.rule == "fqdn"));
    }

    #[test]
    fn hostname_rule_disabled_makes_no_bridge() {
        let mut a = ev("s", "a");
        a.hostname = Some("nas".into());
        let mut b = ev("s", "b");
        b.hostname = Some("nas".into());
        let rules = RulesConfig {
            hostname_enabled: false,
            ..RulesConfig::default()
        };
        let ents = correlate(&[a, b], &rules, &Assertions::default());
        assert_eq!(ents.len(), 2, "hostname rule disabled → no hostname bridge");
    }

    #[test]
    fn host_id_conflict_guard_blocks_weaker_bridge() {
        // Two hosts with the SAME hostname but DIFFERENT host_ids must never
        // merge, even though the hostname rule would otherwise bridge them.
        let mut a = ev("sysinfo", "a");
        a.host_id = Some(hid(0x11));
        a.hostname = Some("web".into());
        let mut b = ev("sysinfo", "b");
        b.host_id = Some(hid(0x22));
        b.hostname = Some("web".into());
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 2, "distinct host_ids must stay separate");
    }

    #[test]
    fn observer_downweights_confidence() {
        let mut a = ev("netring", "a");
        a.observer = Some("netring".into());
        a.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        a.ips = vec!["10.0.0.1".into()];
        let mut b = ev("netring", "b");
        b.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        b.ips = vec!["10.0.0.1".into()];
        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        let c = ents[0]
            .members
            .iter()
            .find(|m| m.rule == "mac_ip")
            .unwrap()
            .confidence;
        assert!(
            (c - 0.8 * 0.8).abs() < 1e-5,
            "observer down-weight: 0.8*0.8, got {c}"
        );
    }

    #[test]
    fn entity_id_host_id_path_is_pinned_prefix() {
        let mut a = ev("sysinfo", "host1");
        a.host_id = Some(hid(0xab)); // "abab…" (64 hex); first 12 chars → "abababababab"
        let ents = correlate(&[a], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents[0].entity_id, "h-abababababab");
    }

    #[test]
    fn entity_id_fallback_path_is_pinned_sha256_prefix() {
        // fqdn-only set → id = h_<sha256("host1.example.com")[..12]>.
        let mut a = ev("s", "a");
        a.fqdn = Some("host1.example.com".into());
        let ents = correlate(&[a], &RulesConfig::default(), &Assertions::default());
        let expected = format!("h-{}", sha256_12("host1.example.com"));
        assert_eq!(ents[0].entity_id, expected);
    }

    #[test]
    fn three_node_transitive_merge() {
        // a≡b by host_id, b≡c by mac_ip → all three in one entity.
        let mut a = ev("sysinfo", "a");
        a.host_id = Some(hid(0x33));
        a.macs = vec!["aa:aa:aa:aa:aa:aa".into()];
        a.ips = vec!["10.0.0.5".into()];
        let mut b = ev("netlink", "a");
        b.host_id = Some(hid(0x33));
        b.macs = vec!["aa:aa:aa:aa:aa:aa".into()];
        b.ips = vec!["10.0.0.5".into()];
        let mut c = ev("netring", "c");
        c.macs = vec!["aa:aa:aa:aa:aa:aa".into()];
        c.ips = vec!["10.0.0.5".into()];
        let ents = correlate(&[a, b, c], &RulesConfig::default(), &Assertions::default());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].members.len(), 3);
    }

    #[test]
    fn shuffled_input_is_byte_identical() {
        let mut a = ev("sysinfo", "a");
        a.host_id = Some(hid(0x44));
        a.ips = vec!["10.0.0.1".into()];
        let mut b = ev("netlink", "b");
        b.host_id = Some(hid(0x44));
        b.macs = vec!["aa:bb:cc:dd:ee:ff".into()];
        let mut c = ev("netring", "c");
        c.fqdn = Some("other.example.com".into());
        let forward = correlate(
            &[a.clone(), b.clone(), c.clone()],
            &RulesConfig::default(),
            &Assertions::default(),
        );
        let reversed = correlate(&[c, b, a], &RulesConfig::default(), &Assertions::default());
        let fj = serde_json::to_string(&forward).unwrap();
        let rj = serde_json::to_string(&reversed).unwrap();
        assert_eq!(fj, rj, "shuffled input must serialize byte-identically");
    }

    // ── Operator assertions (#473, RFC 06 §5.4) ─────────────────────────────

    fn origin(n: u8) -> String {
        format!("h-{:012x}", n)
    }

    fn assertion(kind: AssertionKind, old: &str, new: &str) -> OperatorAssertion {
        OperatorAssertion {
            id: OperatorAssertion::id(kind, old, new),
            kind,
            old: old.to_string(),
            new: new.to_string(),
            asserted_at: 1,
            note: None,
        }
    }

    /// Without an operator, a reinstall is two hosts — and that is *correct*.
    /// This pins the behaviour `link` exists to override, so the test below has
    /// something to be a difference from.
    #[test]
    fn a_reinstall_is_two_hosts_until_an_operator_says_otherwise() {
        let mut a = ev("sysinfo", "web01");
        a.host_id = Some(origin(1));
        a.hostname = Some("web01".into());
        let mut b = ev("sysinfo", "web01");
        b.host_id = Some(origin(2));
        b.hostname = Some("web01".into());

        let ents = correlate(&[a, b], &RulesConfig::default(), &Assertions::default());
        assert_eq!(
            ents.len(),
            2,
            "same hostname, different machine-ids: the conflict guard must refuse \
             to merge them — the catalog cannot tell a reinstall from two machines"
        );
    }

    /// `link` is the operator saying "it is a reinstall". The entity must come
    /// back under the **new** origin, with the old one as an alias — an alias is
    /// what lets a consumer holding the old id find the host again.
    #[test]
    fn link_merges_a_reinstall_under_the_new_origin() {
        let mut a = ev("sysinfo", "web01");
        a.host_id = Some(origin(1));
        let mut b = ev("netlink", "web01");
        b.host_id = Some(origin(2));

        let asserts = Assertions::new([assertion(AssertionKind::Link, &origin(1), &origin(2))]);
        let ents = correlate(&[a, b], &RulesConfig::default(), &asserts);

        assert_eq!(ents.len(), 1, "the operator said these are one machine");
        assert_eq!(
            ents[0].entity_id,
            origin(2),
            "the entity is known by the NEW origin — the old one is history"
        );
        assert_eq!(ents[0].host_id.as_deref(), Some(origin(2).as_str()));
        assert_eq!(
            ents[0].aliases,
            vec![origin(1)],
            "the retired origin must be an alias, or every consumer holding it is stranded"
        );
        assert_eq!(ents[0].members.len(), 2);
    }

    /// `unlink` retires a link: the two must come apart again, and the alias with
    /// them. This is the undo, and it has to be total — an operator who cannot
    /// undo a merge will not risk making one.
    #[test]
    fn unlink_retires_a_link() {
        let mut a = ev("sysinfo", "web01");
        a.host_id = Some(origin(1));
        let mut b = ev("netlink", "web01");
        b.host_id = Some(origin(2));

        let asserts = Assertions::new([
            assertion(AssertionKind::Link, &origin(1), &origin(2)),
            assertion(AssertionKind::Unlink, &origin(1), &origin(2)),
        ]);
        let ents = correlate(&[a, b], &RulesConfig::default(), &asserts);

        assert_eq!(ents.len(), 2, "the veto splits them back apart");
        assert!(
            ents.iter().all(|e| e.aliases.is_empty()),
            "and takes the alias with it — a stale alias would re-point consumers \
             at an entity that no longer claims them"
        );
    }

    /// Order-independence: the same assertions in the other order must give the
    /// same answer. A veto is the operator's *latest* word by construction
    /// (issuing one is how you retire a link), so it wins regardless of the order
    /// documents happen to arrive off the bus — which is not an order we control.
    #[test]
    fn a_veto_wins_whatever_order_the_documents_arrive_in() {
        let mut a = ev("sysinfo", "web01");
        a.host_id = Some(origin(1));
        let mut b = ev("netlink", "web01");
        b.host_id = Some(origin(2));

        let unlink_first = Assertions::new([
            assertion(AssertionKind::Unlink, &origin(1), &origin(2)),
            assertion(AssertionKind::Link, &origin(1), &origin(2)),
        ]);
        let ents = correlate(
            std::slice::from_ref(&a),
            &RulesConfig::default(),
            &unlink_first,
        );
        assert_eq!(ents.len(), 1);

        let ents = correlate(&[a, b], &RulesConfig::default(), &unlink_first);
        assert_eq!(
            ents.len(),
            2,
            "the veto holds no matter which arrived first"
        );
    }

    /// The transitive case, which direct-pair filtering does not see: `link a→c`
    /// and `link b→c` canonicalize a and b to the same id, merging the very pair
    /// the operator vetoed. The veto must still hold.
    #[test]
    fn a_veto_survives_a_link_chain_that_would_route_around_it() {
        let mut a = ev("sysinfo", "a");
        a.host_id = Some(origin(1));
        let mut b = ev("sysinfo", "b");
        b.host_id = Some(origin(2));
        let mut c = ev("sysinfo", "c");
        c.host_id = Some(origin(3));

        let asserts = Assertions::new([
            assertion(AssertionKind::Link, &origin(1), &origin(3)),
            assertion(AssertionKind::Link, &origin(2), &origin(3)),
            assertion(AssertionKind::Unlink, &origin(1), &origin(2)),
        ]);
        let ents = correlate(&[a, b, c], &RulesConfig::default(), &asserts);

        let merged: Vec<_> = ents.iter().filter(|e| e.members.len() > 1).collect();
        for e in &merged {
            let ids: Vec<&str> = e.members.iter().map(|m| m.source.as_str()).collect();
            assert!(
                !(ids.contains(&"a") && ids.contains(&"b")),
                "an operator said a and b are different machines; a chain through c \
                 must not put them in one entity anyway"
            );
        }
    }

    /// A cycle an operator can type (`link a b` + `link b a`) must terminate.
    #[test]
    fn a_link_cycle_terminates() {
        let asserts = Assertions::new([
            assertion(AssertionKind::Link, &origin(1), &origin(2)),
            assertion(AssertionKind::Link, &origin(2), &origin(1)),
        ]);
        // The bound is what is under test: this returns, deterministically.
        let canon = asserts.canon(&origin(1));
        assert!(canon == origin(1) || canon == origin(2));
    }

    /// Chained reinstalls collapse to the newest origin in one pass.
    #[test]
    fn a_link_chain_collapses_to_the_newest_origin() {
        let asserts = Assertions::new([
            assertion(AssertionKind::Link, &origin(1), &origin(2)),
            assertion(AssertionKind::Link, &origin(2), &origin(3)),
        ]);
        assert_eq!(asserts.canon(&origin(1)), origin(3));
        assert_eq!(asserts.aliases_of(&origin(3)), vec![origin(1), origin(2)]);
    }
}
