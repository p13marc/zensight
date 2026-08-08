//! On-demand unit inventory query channel (principle P2, #274).
//!
//! Full per-unit inventory is high-cardinality (hundreds/host) so it is served on
//! demand, never streamed. Mirrors the netlink `@rpc/netlink/*` pattern.
//!
//! Procedures (under `…/@rpc/systemd/`):
//! - `units`            → `Vec<UnitRecord>` (all loaded units)
//! - `failed`           → `Vec<UnitRecord>` (only `active_state == failed`)
//! - `unit?name=<name>` → `UnitDetail` (full props + deps), or `null` if unknown

use std::sync::Arc;

use zensight_common::query_detail::{TimerRecord, UnitDetail, UnitRecord};

use crate::dbus::{ListedUnit, ManagerProxy, ServiceProxy, TimerProxy, UnitProxy};
use crate::events::EventState;

/// Map one `ListUnits` row to a [`UnitRecord`] (pure — unit-testable).
/// `enablement` is the `ListUnitFiles` join built by [`enablement_index`].
pub fn unit_record(
    u: &ListedUnit,
    enablement: &std::collections::HashMap<String, String>,
) -> UnitRecord {
    UnitRecord {
        name: u.0.clone(),
        description: u.1.clone(),
        load_state: u.2.clone(),
        active_state: u.3.clone(),
        sub_state: u.4.clone(),
        job: (!u.8.is_empty()).then(|| u.8.clone()),
        unit_file_state: enablement.get(&u.0).cloned(),
    }
}

/// Index `ListUnitFiles` rows by unit name for the [`unit_record`] join.
///
/// `ListUnitFiles` reports absolute paths (`/usr/lib/systemd/system/nginx.service`)
/// while `ListUnits` reports bare names, so the join key is the path's basename.
/// One D-Bus call covers the whole host — `GetUnitFileState` would be one call
/// per unit, i.e. hundreds.
pub fn enablement_index(
    files: &[crate::dbus::UnitFileEntry],
) -> std::collections::HashMap<String, String> {
    files
        .iter()
        .filter_map(|(path, state)| {
            let name = path.rsplit('/').next()?;
            (!name.is_empty()).then(|| (name.to_string(), state.clone()))
        })
        .collect()
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

/// How many event-ring lines a `@rpc/systemd/unit?name=` reply carries (#274).
const RECENT_CHANGES_MAX: usize = 20;

/// Run the on-demand unit inventory query channel until the session closes.
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    events: EventState,
    cgroup: crate::config::CgroupConfig,
) {
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

    let units_key = zensight_common::command::query_key(&producer, "units");
    let failed_key = zensight_common::command::query_key(&producer, "failed");
    let unit_key = zensight_common::command::query_key(&producer, "unit");
    let events_key = zensight_common::command::query_key(&producer, "events");
    let timers_key = zensight_common::command::query_key(&producer, "timers");
    let cgroups_key = zensight_common::command::query_key(&producer, "cgroups");

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
    let events_q = match session.declare_queryable(&events_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %events_key, "query: declare events failed");
            return;
        }
    };
    let timers_q = match session.declare_queryable(&timers_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %timers_key, "query: declare timers failed");
            return;
        }
    };
    let cgroups_q = match session.declare_queryable(&cgroups_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %cgroups_key, "query: declare cgroups failed");
            return;
        }
    };
    tracing::info!(units = %units_key, failed = %failed_key, unit = %unit_key, events = %events_key,
        timers = %timers_key, cgroups = %cgroups_key, "systemd unit inventory query channel ready");

    loop {
        tokio::select! {
            q = units_q.recv_async() => {
                let Ok(query) = q else { return };
                let recs = list_records(&manager, false).await;
                reply_json(&query, &units_key, &recs).await;
            }
            q = failed_q.recv_async() => {
                let Ok(query) = q else { return };
                let recs = list_records(&manager, true).await;
                reply_json(&query, &failed_key, &recs).await;
            }
            q = unit_q.recv_async() => {
                let Ok(query) = q else { return };
                let name = param(query.parameters().as_str(), "name");
                let mut detail = match name.as_deref() {
                    Some(n) => unit_detail(&conn, &manager, n).await,
                    None => None,
                };
                // This unit's slice of the event ring, newest-first (#274).
                if let (Some(d), Some(n)) = (detail.as_mut(), name.as_deref()) {
                    d.recent_changes = events.recent_for_unit(n, RECENT_CHANGES_MAX);
                }
                reply_json(&query, &unit_key, &detail).await;
            }
            q = events_q.recv_async() => {
                let Ok(query) = q else { return };
                reply_json(&query, &events_key, &events.recent()).await;
            }
            q = timers_q.recv_async() => {
                let Ok(query) = q else { return };
                let now = chrono::Utc::now().timestamp_micros().max(0) as u64;
                let recs = list_timers(&conn, &manager, now).await;
                reply_json(&query, &timers_key, &recs).await;
            }
            q = cgroups_q.recv_async() => {
                let Ok(query) = q else { return };
                let tree = build_cgroup_tree(&cgroup, query.parameters().as_str());
                reply_json(&query, &cgroups_key, &tree).await;
            }
        }
    }
}

/// Build the cgroup subtree for a `@rpc/systemd/cgroups[?path=<rel>]` request (#280).
/// `None` when the path is rejected (traversal) or the subtree doesn't exist.
fn build_cgroup_tree(
    cfg: &crate::config::CgroupConfig,
    params: &str,
) -> Option<zensight_common::query_detail::CgroupNode> {
    let requested = param(params, "path").unwrap_or_else(|| cfg.root.clone());
    let rel = crate::cgroup::sanitize_rel(&requested)?;
    crate::cgroup::build_tree(
        std::path::Path::new(crate::cgroup::CGROUP_ROOT),
        std::path::Path::new(crate::cgroup::PROC_ROOT),
        &rel,
        &cfg.caps(),
    )
}

/// Whether a next-elapse timestamp is in the past (a run is overdue).
fn timer_overdue(next_elapse_usec: u64, now_usec: u64) -> bool {
    next_elapse_usec != 0 && next_elapse_usec != u64::MAX && next_elapse_usec < now_usec
}

/// Enumerate `.timer` units and read their schedule into [`TimerRecord`]s (#279).
async fn list_timers(
    conn: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    now_usec: u64,
) -> Vec<TimerRecord> {
    let listed = match manager.list_units().await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "query: ListUnits (timers) failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for u in listed.iter().filter(|u| u.0.ends_with(".timer")) {
        let (mut last, mut next) = (0u64, 0u64);
        if let Ok(builder) = TimerProxy::builder(conn).path(u.6.clone())
            && let Ok(timer) = builder.build().await
        {
            last = timer.last_trigger_usec().await.unwrap_or(0);
            next = timer.next_elapse_usec_realtime().await.unwrap_or(0);
        }
        out.push(TimerRecord {
            name: u.0.clone(),
            active_state: u.3.clone(),
            last_trigger_usec: last,
            next_elapse_usec: next,
            overdue: timer_overdue(next, now_usec),
        });
    }
    out
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
    // Best-effort: a host that refuses ListUnitFiles still gets its inventory,
    // just without enablement state.
    let enablement = match manager.list_unit_files().await {
        Ok(files) => enablement_index(&files),
        Err(e) => {
            tracing::warn!(error = %e, "query: ListUnitFiles failed; enablement omitted");
            std::collections::HashMap::new()
        }
    };
    listed
        .iter()
        .filter(|u| !failed_only || u.3 == "failed")
        .map(|u| unit_record(u, &enablement))
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
        main_pid: None,
        main_pid_start_time: None,
        // Per-run identity (#303): 16 bytes, hex-encoded; empty/all-zero when
        // the unit isn't running. Matches journald `_SYSTEMD_INVOCATION_ID`.
        invocation_id: unit
            .invocation_id()
            .await
            .ok()
            .filter(|b| !b.is_empty() && b.iter().any(|&x| x != 0))
            .map(hex_lower),
        control_group: None,
    };
    // Service-interface resource accounting is best-effort; uncached (one-shot
    // read, avoids the eager GetAll warning on non-service units).
    if let Ok(svc) = ServiceProxy::builder(conn)
        .path(path)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    {
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
        // Unit ↔ process ↔ log joins (#303): MainPID (0 = not running) with its
        // stat-ticks start time — the reuse-proof `(pid, start_time)` identity —
        // and the cgroup path (the sysinfo `process.cgroup` join key).
        d.main_pid = svc.main_pid().await.ok().filter(|&p| p != 0);
        d.main_pid_start_time = d
            .main_pid
            .and_then(|p| zensight_sensor_core::procutil::proc_start_time_ticks(p as i32));
        d.control_group = svc.control_group().await.ok().filter(|c| !c.is_empty());
    }
    Some(d)
}

/// Lowercase hex of a byte string (InvocationID wire form).
fn hex_lower(bytes: Vec<u8>) -> String {
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Reply on the queryable's own CONCRETE key (never the query's possibly
/// wildcard selector — RFC 05 §2.1).
async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, key: &str, records: &T) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(key, payload).await {
                tracing::warn!(error = %e, key = %key, "query: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query: serialize failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedObjectPath;

    #[test]
    fn invocation_id_hex_encoding() {
        assert_eq!(hex_lower(vec![0xAB, 0x01, 0xFF]), "ab01ff");
        assert_eq!(hex_lower(Vec::new()), "");
    }

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
        let none = std::collections::HashMap::new();
        let r = unit_record(&listed("sshd.service", "active", "start"), &none);
        assert_eq!(r.name, "sshd.service");
        assert_eq!(r.description, "sshd.service desc");
        assert_eq!(r.active_state, "active");
        assert_eq!(r.job.as_deref(), Some("start"));
        // No job → None.
        let r2 = unit_record(&listed("idle.service", "active", ""), &none);
        assert_eq!(r2.job, None);
    }

    #[test]
    fn enablement_joins_list_unit_files_by_basename() {
        let files = vec![
            (
                "/usr/lib/systemd/system/sshd.service".to_string(),
                "enabled".to_string(),
            ),
            (
                "/usr/lib/systemd/system/rescue.service".to_string(),
                "static".to_string(),
            ),
        ];
        let idx = enablement_index(&files);
        assert_eq!(
            unit_record(&listed("sshd.service", "active", ""), &idx).unit_file_state,
            Some("enabled".to_string())
        );
        // A unit with no installed unit file (transient/generated) simply has none.
        assert_eq!(
            unit_record(&listed("session-2.scope", "active", ""), &idx).unit_file_state,
            None
        );
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

    #[test]
    fn timer_overdue_only_for_past_scheduled_elapse() {
        let now = 1_000_000u64;
        assert!(timer_overdue(999_999, now)); // next in the past
        assert!(!timer_overdue(1_000_001, now)); // next in the future
        assert!(!timer_overdue(0, now)); // no next elapse
        assert!(!timer_overdue(u64::MAX, now)); // no next elapse
    }
}
