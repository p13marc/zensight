//! On-demand unit inventory query channel (principle P2, #274).
//!
//! Full per-unit inventory is high-cardinality (hundreds/host) so it is served on
//! demand, never streamed. Mirrors the netlink `@/query/*` pattern.
//!
//! Keys (under `<key_prefix>/@/query/`):
//! - `units`            → `Vec<UnitRecord>` (all loaded units)
//! - `failed`           → `Vec<UnitRecord>` (only `active_state == failed`)
//! - `unit?name=<name>` → `UnitDetail` (full props + deps), or `null` if unknown

use std::sync::Arc;

use zensight_common::query_detail::{UnitDetail, UnitRecord};

use crate::dbus::{ListedUnit, ManagerProxy, ServiceProxy, UnitProxy};

/// Map one `ListUnits` row to a [`UnitRecord`] (pure — unit-testable).
pub fn unit_record(u: &ListedUnit) -> UnitRecord {
    UnitRecord {
        name: u.0.clone(),
        description: u.1.clone(),
        load_state: u.2.clone(),
        active_state: u.3.clone(),
        sub_state: u.4.clone(),
        job: (!u.8.is_empty()).then(|| u.8.clone()),
    }
}

/// Extract a query parameter value from a raw `k=v&k2=v2` parameter string.
fn param(params: &str, key: &str) -> Option<String> {
    params.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// systemd reports unset resource counters as `u64::MAX`.
fn accounting(v: u64) -> Option<u64> {
    (v != u64::MAX).then_some(v)
}

/// Run the on-demand unit inventory query channel until the session closes.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String) {
    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "query: system bus connect failed");
            return;
        }
    };
    let manager = match ManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "query: Manager proxy failed");
            return;
        }
    };

    let units_key = zensight_common::command::query_key(&key_prefix, "units");
    let failed_key = zensight_common::command::query_key(&key_prefix, "failed");
    let unit_key = zensight_common::command::query_key(&key_prefix, "unit");

    let units_q = match session.declare_queryable(&units_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %units_key, "query: declare units failed");
            return;
        }
    };
    let failed_q = match session.declare_queryable(&failed_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %failed_key, "query: declare failed failed");
            return;
        }
    };
    let unit_q = match session.declare_queryable(&unit_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %unit_key, "query: declare unit failed");
            return;
        }
    };
    tracing::info!(units = %units_key, failed = %failed_key, unit = %unit_key,
        "systemd unit inventory query channel ready");

    loop {
        tokio::select! {
            q = units_q.recv_async() => {
                let Ok(query) = q else { return };
                let recs = list_records(&manager, false).await;
                reply_json(&query, &recs).await;
            }
            q = failed_q.recv_async() => {
                let Ok(query) = q else { return };
                let recs = list_records(&manager, true).await;
                reply_json(&query, &recs).await;
            }
            q = unit_q.recv_async() => {
                let Ok(query) = q else { return };
                let name = param(query.parameters().as_str(), "name");
                let detail = match name {
                    Some(n) => unit_detail(&conn, &manager, &n).await,
                    None => None,
                };
                reply_json(&query, &detail).await;
            }
        }
    }
}

/// Collect the unit inventory, optionally filtered to failed units only.
async fn list_records(manager: &ManagerProxy<'_>, failed_only: bool) -> Vec<UnitRecord> {
    let listed = match manager.list_units().await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "query: ListUnits failed");
            return Vec::new();
        }
    };
    listed
        .iter()
        .filter(|u| !failed_only || u.3 == "failed")
        .map(unit_record)
        .collect()
}

/// Build the full [`UnitDetail`] for `name`, or `None` if it can't be resolved.
async fn unit_detail(
    conn: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    name: &str,
) -> Option<UnitDetail> {
    let path = manager.load_unit(name).await.ok()?;
    let unit = UnitProxy::builder(conn)
        .path(path.clone())
        .ok()?
        .build()
        .await
        .ok()?;

    let mut d = UnitDetail {
        name: name.to_string(),
        description: unit.description().await.unwrap_or_default(),
        load_state: unit.load_state().await.ok()?,
        active_state: unit.active_state().await.ok()?,
        sub_state: unit.sub_state().await.unwrap_or_default(),
        fragment_path: unit.fragment_path().await.ok().filter(|p| !p.is_empty()),
        active_enter_usec: unit.active_enter_timestamp().await.unwrap_or(0),
        n_restarts: 0,
        mem_bytes: None,
        cpu_usec: None,
        tasks: None,
        exec_main_status: 0,
        requires: unit.requires().await.unwrap_or_default(),
        wants: unit.wants().await.unwrap_or_default(),
        after: unit.after().await.unwrap_or_default(),
        before: unit.before().await.unwrap_or_default(),
        recent_changes: Vec::new(),
    };
    // Service-interface resource accounting is best-effort.
    if let Ok(svc) = ServiceProxy::builder(conn).path(path).ok()?.build().await {
        d.n_restarts = svc.n_restarts().await.unwrap_or(0);
        d.exec_main_status = svc.exec_main_status().await.unwrap_or(0);
        d.mem_bytes = svc.memory_current().await.ok().and_then(accounting);
        d.cpu_usec = svc
            .cpu_usage_nsec()
            .await
            .ok()
            .and_then(accounting)
            .map(|ns| ns / 1000);
        d.tasks = svc.tasks_current().await.ok().and_then(accounting);
    }
    Some(d)
}

async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, records: &T) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                tracing::warn!(error = %e, "query: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query: serialize failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedObjectPath;

    fn listed(name: &str, active: &str, job: &str) -> ListedUnit {
        (
            name.to_string(),
            format!("{name} desc"),
            "loaded".to_string(),
            active.to_string(),
            "running".to_string(),
            String::new(),
            OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/x").unwrap(),
            0,
            job.to_string(),
            OwnedObjectPath::try_from("/").unwrap(),
        )
    }

    #[test]
    fn unit_record_maps_fields_and_job() {
        let r = unit_record(&listed("sshd.service", "active", "start"));
        assert_eq!(r.name, "sshd.service");
        assert_eq!(r.description, "sshd.service desc");
        assert_eq!(r.active_state, "active");
        assert_eq!(r.job.as_deref(), Some("start"));
        // No job → None.
        let r2 = unit_record(&listed("idle.service", "active", ""));
        assert_eq!(r2.job, None);
    }

    #[test]
    fn param_parses_name() {
        assert_eq!(
            param("name=sshd.service", "name").as_deref(),
            Some("sshd.service")
        );
        assert_eq!(
            param("foo=1&name=a.timer&bar=2", "name").as_deref(),
            Some("a.timer")
        );
        assert_eq!(param("other=x", "name"), None);
        assert_eq!(param("", "name"), None);
    }

    #[test]
    fn accounting_normalizes_unset() {
        assert_eq!(accounting(u64::MAX), None);
        assert_eq!(accounting(42), Some(42));
    }
}
