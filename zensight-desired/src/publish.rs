//! Publishing the compiled documents, and only the ones that changed (#938).

use std::collections::BTreeMap;
use std::sync::Arc;

use zenoh::Session;
use zenoh::pubsub::Publisher;

use crate::compile::{Compiled, Desired};

/// What one pass did, for the log line and for `plan`'s output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: usize,
}

impl PassReport {
    pub fn wrote_nothing(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.deleted.is_empty()
    }
}

/// Holds the declared publishers and the last-published bytes per key.
///
/// One publisher per key, cached: `session.put` is CI-banned, and a publisher
/// declared per pass would churn declarations across the fleet every refresh.
pub struct Publisher0 {
    session: Arc<Session>,
    publishers: BTreeMap<String, Publisher<'static>>,
    /// Canonical bytes last written per key — the diff that decides whether a
    /// pass writes at all.
    published: BTreeMap<String, String>,
    /// Consecutive passes a key has gone unproduced. A key is deleted only
    /// after `grace` of them.
    missing_for: BTreeMap<String, u32>,
    grace: u32,
}

impl Publisher0 {
    pub fn new(session: Arc<Session>, grace: u32) -> Self {
        Publisher0 {
            session,
            publishers: BTreeMap::new(),
            published: BTreeMap::new(),
            missing_for: BTreeMap::new(),
            grace: grace.max(1),
        }
    }

    /// Seed the diff from what the storage already holds, so a **restart**
    /// publishes nothing.
    ///
    /// Without this the in-memory `published` map starts empty and the first
    /// pass after every restart rewrites every document on every host — the
    /// fleet's `applied/<topic>` markers would show a permanent reconvergence
    /// and the operator would have no way to tell a real change from a
    /// bounce.
    pub async fn seed(&mut self, timeout: std::time::Duration) {
        let selector = format!("v1/{}/state/**", crate::ORIGIN);
        // Default consolidation here, unlike `fleet::fetch` — and the
        // asymmetry is deliberate. This GET only needs *a* prior value per key
        // to diff against: if a handover hands back a stale one, the first
        // pass republishes that document once and the diff is correct
        // thereafter. Self-correcting, so collapsing by key is the cheaper
        // read. The fleet GET has no such recovery — a wrong entity set
        // produces wrong documents, silently — which is why that one turns
        // consolidation off and decides by `last_updated`.
        let Ok(replies) = self
            .session
            .get(&selector)
            .target(zenoh::query::QueryTarget::All)
            .timeout(timeout)
            .await
        else {
            tracing::warn!(
                selector = %selector,
                "could not read the @desired storage; the first pass will republish \
                 everything the policy yields"
            );
            return;
        };
        let mut n = 0;
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result()
                && let Ok(v) =
                    serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes())
            {
                self.published.insert(
                    sample.key_expr().as_str().to_string(),
                    crate::merge::canonical(&v),
                );
                n += 1;
            }
        }
        tracing::info!(documents = n, "seeded from the @desired storage");
    }

    async fn publisher_for(&mut self, key: &str) -> anyhow::Result<&Publisher<'static>> {
        if !self.publishers.contains_key(key) {
            // Desired state is operator policy: it must arrive, so reliable +
            // block, the same class the catalog uses for entities.
            let q = zensight_common::QosClass::Entity;
            let p = self
                .session
                .declare_publisher(key.to_string())
                .congestion_control(q.congestion_control())
                .priority(q.priority())
                .express(q.express())
                .reliability(q.reliability())
                .await
                .map_err(|e| anyhow::anyhow!("declare publisher {key}: {e}"))?;
            self.publishers.insert(key.to_string(), p);
        }
        Ok(self.publishers.get(key).expect("just inserted"))
    }

    /// Apply one compiled pass.
    pub async fn apply(&mut self, compiled: &Compiled) -> PassReport {
        let mut report = PassReport::default();

        let mut want: BTreeMap<String, &Desired> = BTreeMap::new();
        for d in compiled.docs.values() {
            match host_key(d) {
                Some(k) => {
                    want.insert(k, d);
                }
                None => tracing::warn!(
                    host = %d.host,
                    "not a valid host id — no key can be built for it"
                ),
            }
        }

        for (key, d) in &want {
            self.missing_for.remove(key);
            if self.published.get(key) == Some(&d.canonical) {
                report.unchanged += 1;
                continue;
            }
            let is_new = !self.published.contains_key(key);
            let payload = d.canonical.clone();
            match self.publisher_for(key).await {
                Ok(p) => match p.put(payload.clone().into_bytes()).await {
                    Ok(()) => {
                        self.published.insert(key.clone(), payload);
                        if is_new {
                            report.added.push(key.clone());
                        } else {
                            report.changed.push(key.clone());
                        }
                    }
                    Err(e) => tracing::error!(key = %key, error = %e, "publish failed"),
                },
                Err(e) => tracing::error!(key = %key, error = %e, "publisher failed"),
            }
        }

        // Deletion, and the two guards on it.
        //
        // A document goes only when the policy stopped yielding it for a host
        // **that is still in the catalog** — never because the catalog stopped
        // showing the host. That is the load-bearing half: a failed or slow
        // catalog GET empties the compiled set, and without this check every
        // document in the fleet would become a deletion candidate at once, on
        // a timer. The grace then covers the case where the host IS here and
        // the policy briefly did not yield for it.
        //
        // A key whose host is absent is held indefinitely and does not even
        // accrue grace: "I cannot see it" is not "it should have no
        // configuration", and the sensor is reconciling that document quite
        // happily meanwhile.
        let gone: Vec<String> = self
            .published
            .keys()
            .filter(|k| !want.contains_key(*k))
            .filter(|k| host_of_key(k).is_some_and(|h| compiled.hosts_seen.contains(h)))
            .cloned()
            .collect();
        for key in gone {
            let n = self.missing_for.entry(key.clone()).or_insert(0);
            *n += 1;
            if *n < self.grace {
                tracing::info!(
                    key = %key, pass = *n, grace = self.grace,
                    "no longer produced; holding before deleting"
                );
                continue;
            }
            match self.publisher_for(&key).await {
                Ok(p) => match p.delete().await {
                    Ok(()) => {
                        self.published.remove(&key);
                        self.missing_for.remove(&key);
                        self.publishers.remove(&key); // drop -> undeclare
                        report.deleted.push(key);
                    }
                    Err(e) => tracing::error!(key = %key, error = %e, "tombstone failed"),
                },
                Err(e) => tracing::error!(key = %key, error = %e, "publisher failed"),
            }
        }

        report
    }
}

/// The host chunk of a `@desired` key: `v1/@desired/state/<host>/…`.
///
/// Read from the key rather than tracked beside it, so a document seeded from
/// the storage at startup — which arrives as a key and bytes, with no
/// provenance — is subject to the same deletion guard as one this process
/// published itself. A key it cannot parse is never deleted, which is the safe
/// direction.
fn host_of_key(key: &str) -> Option<&str> {
    let mut parts = key.split('/');
    let (v1, origin, class, host) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (v1 == "v1" && origin == crate::ORIGIN && class == "state").then_some(host)
}

/// The concrete key for one compiled document, or `None` if the host chunk is
/// not a valid host id.
pub fn host_key(d: &Desired) -> Option<String> {
    let host = zenkey::origin::HostId::parse(&d.host).ok()?;
    let spec = zensight_common::desired::topic(&d.producer, &d.topic)?;
    Some(spec.key(&host).to_string())
}
