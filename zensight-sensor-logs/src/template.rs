//! Streaming log-template mining (#102): a Drain-style fixed-depth parse-tree
//! miner that turns free-form log lines into stable *templates*.
//!
//! For every message we first **mask** obvious variables (integers, IPs, UUIDs,
//! hex blobs, timestamps, paths, numbers-with-units) into typed placeholders,
//! then cluster the masked token sequence with a fixed-depth tree (Drain): group
//! by token count, descend a few leading tokens, and within the candidate group
//! pick the cluster whose template is similar enough; matching positions that
//! differ collapse to the wildcard `<*>`. Each cluster yields a `template_id`
//! (a stable FNV-1a hash of its masked template) and the masked `template`
//! string itself.
//!
//! The [`TemplateMiner`] core is **pure and deterministic** (no clock, no RNG):
//! given the same sequence of inputs it always produces the same templates and
//! ids, which makes it directly unit-testable. [`TemplateAggregator`] wraps it
//! behind a `Mutex` and tracks bounded per-template counters for the derived
//! `logs/by_template/*` series, mirroring the per-unit rollups in [`crate::derived`].

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use regex::Regex;

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Wildcard token for positions that vary within a cluster.
const WILDCARD: &str = "<*>";

/// Overflow bucket name for templates beyond the cardinality cap.
const OTHER_TEMPLATE: &str = "other";

// ---------------------------------------------------------------------------
// Masking
// ---------------------------------------------------------------------------
//
// Each regex replaces a recognized *variable* span with a typed placeholder
// BEFORE tokenizing. Placeholders are concrete tokens (e.g. `<IP>`) so two lines
// that differ only in their variables mask to the *same* string and therefore
// the same template/id. Order matters: more specific patterns run first so a
// UUID is not first chewed up by the bare-number rule, etc. Placeholders contain
// `<>` and uppercase letters only, so later digit/path rules never disturb them.

static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap()
});

// ISO-8601 / RFC-3339 style date-times, then bare clock times.
static TIMESTAMP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b\d{4}-\d{2}-\d{2}[t ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:z|[+-]\d{2}:?\d{2})?\b")
        .unwrap()
});
static TIME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{2}:\d{2}:\d{2}(?:\.\d+)?\b").unwrap());

// MAC address (before IPv6, which also uses colons + hex).
static MAC_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:[0-9a-f]{2}:){5}[0-9a-f]{2}\b").unwrap());

// IPv6 (a pragmatic matcher: runs of hex groups separated by colons, incl. `::`).
static IPV6_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:[0-9a-f]{0,4}:){2,7}[0-9a-f]{0,4}\b").unwrap());

// IPv4 (optionally with a /CIDR suffix).
static IPV4_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}(?:/\d{1,2})?\b").unwrap());

// Hex blobs: `0x…` and bare runs of 8+ hex digits.
static HEX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b0x[0-9a-f]+\b|\b[0-9a-f]{8,}\b").unwrap());

// Filesystem paths: two-or-more `/segment` runs (avoids eating a lone `/`).
static PATH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:/[A-Za-z0-9_.\-]+){2,}/?").unwrap());

// Numbers with a unit suffix (durations, sizes, percentages, rates).
static NUM_UNIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b\d+(?:\.\d+)?(?:ns|us|ms|s|m|h|d|kib|mib|gib|tib|kb|mb|gb|tb|bps|kbps|mbps|gbps|b|%)\b",
    )
    .unwrap()
});

// Any remaining bare integer/float (optionally signed).
static NUM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-?\b\d+(?:\.\d+)?\b").unwrap());

/// Mask variable spans in `content` into typed placeholders. Pure.
pub fn mask(content: &str) -> String {
    let s = UUID_RE.replace_all(content, "<UUID>");
    let s = TIMESTAMP_RE.replace_all(&s, "<TS>");
    let s = MAC_RE.replace_all(&s, "<MAC>");
    let s = IPV6_RE.replace_all(&s, "<IP>");
    let s = IPV4_RE.replace_all(&s, "<IP>");
    let s = TIME_RE.replace_all(&s, "<TS>");
    let s = HEX_RE.replace_all(&s, "<HEX>");
    let s = PATH_RE.replace_all(&s, "<PATH>");
    let s = NUM_UNIT_RE.replace_all(&s, "<NUM>");
    let s = NUM_RE.replace_all(&s, "<NUM>");
    s.into_owned()
}

/// FNV-1a 64-bit hash — small, fast, deterministic; used for `template_id`.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Stable id string for a masked template (16-char lowercase hex).
pub fn template_id(template: &str) -> String {
    format!("{:016x}", fnv1a_64(template))
}

/// True for masked/wildcard tokens (`<IP>`, `<*>`, …). Such tokens share a single
/// `<*>` branch in the tree so variable-*leading* lines still cluster together.
fn is_variable(token: &str) -> bool {
    token.starts_with('<') && token.ends_with('>')
}

/// The branch key for a token during tree descent: variable tokens collapse to
/// the wildcard branch, everything else branches by its literal value.
fn branch_key(token: &str) -> &str {
    if is_variable(token) { WILDCARD } else { token }
}

// ---------------------------------------------------------------------------
// Drain parameters
// ---------------------------------------------------------------------------

/// Tunable Drain parameters. Defaults follow the logpai/Drain3 conventions.
#[derive(Debug, Clone, Copy)]
pub struct DrainParams {
    /// Max number of token layers descended below the length layer (>=1).
    pub depth: usize,
    /// Minimum fraction of matching non-wildcard tokens to join a cluster.
    pub sim_threshold: f64,
    /// Max distinct literal children per tree node before new tokens fold into
    /// the `<*>` branch (keeps the tree fan-out bounded).
    pub max_children: usize,
    /// Hard cap on the number of clusters retained (bounds memory).
    pub max_clusters: usize,
}

impl Default for DrainParams {
    fn default() -> Self {
        Self {
            depth: 4,
            sim_threshold: 0.4,
            max_children: 100,
            max_clusters: 1000,
        }
    }
}

/// Result of mining one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedTemplate {
    /// Stable FNV-1a id of the masked template.
    pub id: String,
    /// The masked template string (variable positions are `<*>` / typed masks).
    pub template: String,
}

/// One log group: a template token sequence (with `<*>` wildcards).
#[derive(Debug)]
struct Cluster {
    tokens: Vec<String>,
}

impl Cluster {
    fn template(&self) -> String {
        self.tokens.join(" ")
    }
}

/// A fixed-depth Drain parse-tree node.
#[derive(Debug, Default)]
struct Node {
    children: HashMap<String, Node>,
    /// Cluster indices anchored at this (leaf) node.
    clusters: Vec<usize>,
}

/// Drain-style streaming template miner. Pure & deterministic.
#[derive(Debug)]
pub struct TemplateMiner {
    params: DrainParams,
    /// First tree layer: token-count → subtree.
    by_length: HashMap<usize, Node>,
    clusters: Vec<Cluster>,
}

impl TemplateMiner {
    pub fn new(params: DrainParams) -> Self {
        Self {
            params: DrainParams {
                depth: params.depth.max(1),
                sim_threshold: params.sim_threshold.clamp(0.0, 1.0),
                max_children: params.max_children.max(1),
                max_clusters: params.max_clusters.max(1),
            },
            by_length: HashMap::new(),
            clusters: Vec::new(),
        }
    }

    /// Number of distinct clusters mined so far (for tests / introspection).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// Mine one raw log line: mask, tokenize, descend the tree, then match or
    /// create a cluster. Returns the resulting template id + string.
    pub fn add_log_message(&mut self, content: &str) -> MinedTemplate {
        let masked = mask(content);
        let tokens: Vec<String> = masked.split_whitespace().map(str::to_string).collect();

        // Empty / whitespace-only line: a degenerate but stable template.
        if tokens.is_empty() {
            return MinedTemplate {
                id: template_id(""),
                template: String::new(),
            };
        }

        let n = tokens.len();
        let depth = self.params.depth.min(n);
        let max_children = self.params.max_children;

        // Descend: length layer, then up to `depth` leading-token layers.
        let mut node = self.by_length.entry(n).or_default();
        for token in tokens.iter().take(depth) {
            let key = branch_key(token).to_string();
            if !node.children.contains_key(&key) {
                // Fold into the wildcard branch once the literal fan-out is full.
                if key != WILDCARD && node.children.len() >= max_children {
                    node.children.entry(WILDCARD.to_string()).or_default();
                    node = node.children.get_mut(WILDCARD).unwrap();
                    continue;
                }
                node.children.insert(key.clone(), Node::default());
            }
            node = node.children.get_mut(&key).unwrap();
        }

        // Pick the most similar candidate cluster in this leaf.
        let threshold = self.params.sim_threshold;
        let mut best: Option<(usize, f64)> = None;
        for &idx in &node.clusters {
            let sim = seq_sim(&self.clusters[idx].tokens, &tokens);
            if sim >= threshold && best.is_none_or(|(_, b)| sim > b) {
                best = Some((idx, sim));
            }
        }

        if let Some((idx, _)) = best {
            merge_template(&mut self.clusters[idx].tokens, &tokens);
            let template = self.clusters[idx].template();
            return MinedTemplate {
                id: template_id(&template),
                template,
            };
        }

        // No match. Create a new cluster unless we are at the hard cap, in which
        // case we still return a stable id for the masked line (just untracked).
        if self.clusters.len() >= self.params.max_clusters {
            let template = tokens.join(" ");
            return MinedTemplate {
                id: template_id(&template),
                template,
            };
        }
        let idx = self.clusters.len();
        self.clusters.push(Cluster {
            tokens: tokens.clone(),
        });
        node.clusters.push(idx);
        let template = tokens.join(" ");
        MinedTemplate {
            id: template_id(&template),
            template,
        }
    }
}

/// Drain similarity: fraction of matching non-wildcard template positions.
/// Wildcard (`<*>`) template positions match anything but are not counted as a
/// hit (Drain semantics); the denominator is the full template length.
fn seq_sim(template: &[String], tokens: &[String]) -> f64 {
    if template.is_empty() {
        return 1.0;
    }
    let mut matches = 0usize;
    for (t1, t2) in template.iter().zip(tokens.iter()) {
        if t1 == WILDCARD {
            continue;
        }
        if t1 == t2 {
            matches += 1;
        }
    }
    matches as f64 / template.len() as f64
}

/// Generalize a cluster's template against a newly matched line: positions whose
/// tokens differ (and are not already `<*>`) collapse to the wildcard.
fn merge_template(template: &mut [String], tokens: &[String]) {
    for (slot, tok) in template.iter_mut().zip(tokens.iter()) {
        if slot != tok && slot != WILDCARD {
            *slot = WILDCARD.to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregator: bounded per-template counters for derived telemetry
// ---------------------------------------------------------------------------

/// Per-template cumulative counters.
#[derive(Debug, Default, Clone, Copy)]
struct TemplateCounts {
    count: u64,
    errors: u64,
}

#[derive(Debug)]
struct TemplateInner {
    miner: TemplateMiner,
    /// `template_id` → counters, bounded to `top_templates` + `other`.
    counts: HashMap<String, TemplateCounts>,
}

/// Mines templates for the live log stream and tracks bounded per-template
/// counters for the `logs/by_template/*` derived series. Shared (`Arc`) between
/// the publish loop (which calls [`observe`](Self::observe)) and the emit tick.
pub struct TemplateAggregator {
    top_templates: usize,
    inner: Mutex<TemplateInner>,
}

impl TemplateAggregator {
    pub fn new(params: DrainParams, top_templates: usize) -> Self {
        Self {
            top_templates: top_templates.max(1),
            inner: Mutex::new(TemplateInner {
                miner: TemplateMiner::new(params),
                counts: HashMap::new(),
            }),
        }
    }

    /// Mine `content`, fold it into the bounded per-template counters, and return
    /// the template id + masked template for attaching as per-line labels.
    ///
    /// `is_error` marks the line as an error/critical so the `errors_total`
    /// series can track it. Returns `None` only if the lock is poisoned.
    pub fn observe(&self, content: &str, is_error: bool) -> Option<MinedTemplate> {
        let mut inner = self.inner.lock().ok()?;
        let mined = inner.miner.add_log_message(content);

        // Bounded counters: a new template folds into `other` once the cap is
        // reached, so the emitted series set never grows unbounded.
        let cap = self.top_templates;
        let key = if inner.counts.contains_key(&mined.id) || inner.counts.len() < cap {
            mined.id.clone()
        } else {
            OTHER_TEMPLATE.to_string()
        };
        let entry = inner.counts.entry(key).or_default();
        entry.count += 1;
        if is_error {
            entry.errors += 1;
        }
        Some(mined)
    }

    /// Snapshot the per-template counters into telemetry points published under
    /// `zensight/syslog/<source>/logs/by_template/<id>/{count,errors}_total`.
    pub fn emit(&self, source: &str) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let Ok(inner) = self.inner.lock() else {
            return points;
        };
        let counter = |metric: String, v: u64| {
            TelemetryPoint::new(source, Protocol::Syslog, metric, TelemetryValue::Counter(v))
        };
        for (id, c) in &inner.counts {
            points.push(counter(
                format!("logs/by_template/{id}/count_total"),
                c.count,
            ));
            if c.errors > 0 {
                points.push(counter(
                    format!("logs/by_template/{id}/errors_total"),
                    c.errors,
                ));
            }
        }
        points
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn miner() -> TemplateMiner {
        TemplateMiner::new(DrainParams::default())
    }

    #[test]
    fn masks_ip_number_and_uuid() {
        assert_eq!(mask("Connection from 192.168.1.1"), "Connection from <IP>");
        assert_eq!(mask("retrying in 30 seconds"), "retrying in <NUM> seconds");
        assert_eq!(
            mask("session 550e8400-e29b-41d4-a716-446655440000 closed"),
            "session <UUID> closed"
        );
        assert_eq!(mask("took 125ms to finish"), "took <NUM> to finish");
        assert_eq!(mask("reading /var/log/syslog now"), "reading <PATH> now");
        assert_eq!(mask("ptr 0xdeadbeef freed"), "ptr <HEX> freed");
        assert_eq!(mask("at 2023-08-24T05:14:15Z done"), "at <TS> done");
    }

    #[test]
    fn masking_is_stable_and_deterministic() {
        // Same input twice → identical mask (pure).
        assert_eq!(mask("user 5 did 7 things"), mask("user 5 did 7 things"));
        assert_eq!(mask("user 5 did 7 things"), "user <NUM> did <NUM> things");
    }

    #[test]
    fn identical_structure_different_vars_same_id() {
        let mut m = miner();
        let a = m.add_log_message("Connection from 192.168.1.1 port 22");
        let b = m.add_log_message("Connection from 10.0.0.2 port 443");
        assert_eq!(a.id, b.id, "same structure must share a template id");
        assert_eq!(a.template, b.template);
        // Both variables masked → no wildcard merge needed.
        assert_eq!(a.template, "Connection from <IP> port <NUM>");
        // Only one cluster was created.
        assert_eq!(m.cluster_count(), 1);
    }

    #[test]
    fn different_structure_different_ids() {
        let mut m = miner();
        let a = m.add_log_message("user alice logged in successfully");
        let b = m.add_log_message("disk failure on controller two now");
        assert_ne!(a.id, b.id);
        assert_eq!(m.cluster_count(), 2);
    }

    #[test]
    fn merge_introduces_wildcard_at_varying_position() {
        let mut m = miner();
        // Same length & shared depth-prefix (first `depth`=4 tokens identical),
        // differing only in a later non-masked word → merge to `<*>`. (A token
        // that varies *within* the prefix would branch to a separate leaf — that
        // is faithful Drain behavior, see `different_structure_different_ids`.)
        let _ = m.add_log_message("backup job finished for database orders");
        let b = m.add_log_message("backup job finished for database users");
        assert_eq!(b.template, "backup job finished for database <*>");
        assert!(b.template.contains("<*>"));
        // Still one cluster (they merged), and a third matching line is stable.
        assert_eq!(m.cluster_count(), 1);
        let c = m.add_log_message("backup job finished for database sessions");
        assert_eq!(c.template, "backup job finished for database <*>");
        assert_eq!(b.id, c.id);
    }

    #[test]
    fn template_id_is_stable_hash_of_template() {
        // Deterministic, independent of the miner.
        assert_eq!(
            template_id("user <*> logged in"),
            template_id("user <*> logged in")
        );
        assert_ne!(template_id("a b c"), template_id("a b d"));
    }

    #[test]
    fn different_lengths_do_not_merge() {
        let mut m = miner();
        let a = m.add_log_message("service started");
        let b = m.add_log_message("service started cleanly now");
        assert_ne!(a.id, b.id);
        assert_eq!(m.cluster_count(), 2);
    }

    #[test]
    fn max_clusters_bounds_growth() {
        let mut m = TemplateMiner::new(DrainParams {
            max_clusters: 2,
            ..DrainParams::default()
        });
        // Three structurally-distinct lines, cap of 2 → only 2 clusters retained.
        m.add_log_message("alpha bravo charlie delta");
        m.add_log_message("echo foxtrot golf hotel");
        let third = m.add_log_message("india juliet kilo lima");
        assert_eq!(m.cluster_count(), 2);
        // The over-cap line still gets a stable id.
        assert_eq!(third.id, template_id("india juliet kilo lima"));
    }

    #[test]
    fn aggregator_emits_per_template_counters() {
        let agg = TemplateAggregator::new(DrainParams::default(), 10);
        agg.observe("Connection from 192.168.1.1", false).unwrap();
        agg.observe("Connection from 10.0.0.2", true).unwrap();
        let id = template_id("Connection from <IP>");
        let pts = agg.emit("host01");
        let count = pts
            .iter()
            .find(|p| p.metric == format!("logs/by_template/{id}/count_total"))
            .unwrap();
        assert_eq!(count.value, TelemetryValue::Counter(2));
        let errors = pts
            .iter()
            .find(|p| p.metric == format!("logs/by_template/{id}/errors_total"))
            .unwrap();
        assert_eq!(errors.value, TelemetryValue::Counter(1));
    }

    #[test]
    fn aggregator_returns_labels_for_each_line() {
        let agg = TemplateAggregator::new(DrainParams::default(), 10);
        let mined = agg.observe("worker 7 finished job 12", false).unwrap();
        assert_eq!(mined.template, "worker <NUM> finished job <NUM>");
        assert_eq!(mined.id, template_id("worker <NUM> finished job <NUM>"));
    }

    #[test]
    fn aggregator_bounds_templates_to_top_n_plus_other() {
        let agg = TemplateAggregator::new(DrainParams::default(), 2);
        // Five structurally-distinct templates, cap 2 → 2 tracked + `other`.
        for i in 0..5 {
            // Distinct leading words so each is its own template.
            let line = format!("alpha{i} beta gamma delta");
            agg.observe(&line, false).unwrap();
        }
        let pts = agg.emit("h");
        let series = pts
            .iter()
            .filter(|p| {
                p.metric.starts_with("logs/by_template/") && p.metric.ends_with("/count_total")
            })
            .count();
        assert_eq!(series, 3, "2 tracked templates + the `other` bucket");
        assert!(
            pts.iter()
                .any(|p| p.metric.starts_with("logs/by_template/other/count_total"))
        );
    }
}
