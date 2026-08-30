//! The explorer's pump: the task that owns the [`zenkey_fleet::Monitor`]
//! (#748).
//!
//! The GUI cannot own the monitor directly — `Monitor::shutdown(self)`
//! consumes it, and an `App` field cannot be consumed from `update`. So a
//! `Task::stream` owns it, `App` holds only an [`ExplorerCtl`] command
//! handle, and teardown is the pump's acknowledged `shutdown().await` (never
//! a bare drop: the next monitor over the same keys must not race the old
//! subscribers' undeclares).
//!
//! Throttling is the pump's other job: per-sample work (the QoS fold) stays
//! here, off the GUI thread, and the GUI receives one
//! [`Message::ExplorerTick`] per monitor stats tick (250 ms — the cadence
//! the GUI already redraws on) regardless of bus rate.

use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use zenkey_fleet::{Monitor, MonitorSpec, RetentionBudget, WatchId};
use zensight_common::keyexpr::{all_liveliness_wildcard, correlator_alive_key};

use crate::message::Message;

use super::core::{EXPLORER_CAPACITY, EXPLORER_MAX_KEYS, ExplorerCore, RETAIN_BYTES, RETAIN_SECS};
use super::inspector::InspectedSample;

/// Commands the GUI sends the pump.
#[derive(Debug)]
pub enum ExplorerCmd {
    /// Declare a data-plane watch for this selector.
    Watch(String),
    /// Release a watch.
    Unwatch(WatchId),
    /// Select (or clear) the key whose retained sample the tick snapshot
    /// should carry.
    Inspect(Option<String>),
    /// Acknowledged teardown.
    Shutdown,
}

/// The GUI's handle on a running pump. `Clone` because `Message` is; the
/// pump also stops when every handle is dropped.
#[derive(Debug, Clone)]
pub struct ExplorerCtl(tokio::sync::mpsc::UnboundedSender<ExplorerCmd>);

impl ExplorerCtl {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ExplorerCmd>) -> ExplorerCtl {
        ExplorerCtl(tx)
    }

    pub fn send(&self, cmd: ExplorerCmd) {
        // A send to a finished pump is not an error: the stream's end has
        // already told the GUI it stopped.
        let _ = self.0.send(cmd);
    }
}

/// The spec every explorer monitor starts from: **lazy** (no data-plane
/// subscribers until the user watches something), with both liveliness
/// sweeps — the fleet wildcard, and the catalog's verbatim token that `*`
/// can never match (grammar D4). Watching only the first renders "catalog
/// dead" and "no entities" identically, the false verdict RFC 05 §3.1
/// forbids.
fn spec() -> MonitorSpec {
    MonitorSpec {
        selectors: Vec::new(),
        liveliness: vec![all_liveliness_wildcard(), correlator_alive_key()],
        stats_tick: Duration::from_millis(250),
        capacity: EXPLORER_CAPACITY,
        max_keys: EXPLORER_MAX_KEYS,
    }
}

/// Run a monitor on the GUI's session until told to stop.
///
/// The session is borrowed exactly the way the fleet view's queriers borrow
/// it (`Fleet::new(&session, "")`, #745): the GUI never opens a second
/// session and never calls `zenkey_fleet::open` (#466). Selectors handed to
/// `watch` are base-relative — under a namespaced deployment the session
/// strips the base, so `SampleView.key` arrives base-relative too and the
/// view's caption says "as this session sees it" rather than "the wire".
pub fn run(session: Arc<zenoh::Session>) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let monitor = match Monitor::start(&session, spec()).await {
            Ok(m) => m,
            Err(e) => {
                yield Message::ExplorerError(format!("monitor start failed: {e}"));
                yield Message::ExplorerStopped;
                return;
            }
        };
        monitor.core().set_retention_budget(RetentionBudget {
            max_bytes: RETAIN_BYTES,
            max_age: Duration::from_secs(RETAIN_SECS),
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        yield Message::ExplorerStarted(ExplorerCtl(tx));

        let mut events = monitor.events();
        let mut core = ExplorerCore::default();
        let mut inspect: Option<String> = None;

        loop {
            // Compute the message outside the select arms — `yield` inside a
            // `select!` branch does not compose with `async_stream`.
            let out: Option<Message> = tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(ExplorerCmd::Watch(selector)) => {
                        match monitor.watch(&selector).await {
                            Ok(_) => None,
                            Err(e) => Some(Message::ExplorerError(format!(
                                "watch {selector:?} failed: {e}"
                            ))),
                        }
                    }
                    Some(ExplorerCmd::Unwatch(id)) => {
                        match monitor.unwatch(id).await {
                            Ok(()) => None,
                            Err(e) => Some(Message::ExplorerError(format!(
                                "unwatch failed: {e}"
                            ))),
                        }
                    }
                    Some(ExplorerCmd::Inspect(key)) => {
                        inspect = key;
                        None
                    }
                    Some(ExplorerCmd::Shutdown) | None => break,
                },
                item = events.recv() => match item {
                    None => break,
                    Some(item) => {
                        let is_tick = matches!(
                            &item,
                            zenkey_fleet::StreamItem::Event(zenkey_fleet::FleetEvent::StatsTick)
                        );
                        core.apply(&item);
                        if is_tick {
                            let inspected = inspect.as_deref().and_then(|key| {
                                latest_retained(&monitor, key, &mut core)
                            });
                            let watches = monitor.watched().await;
                            Some(Message::ExplorerTick(Arc::new(
                                core.snapshot(monitor.core(), watches, inspected),
                            )))
                        } else {
                            None
                        }
                    }
                },
            };
            if let Some(msg) = out {
                yield msg;
            }
        }

        if let Err(e) = monitor.shutdown().await {
            yield Message::ExplorerError(format!("monitor shutdown: {e}"));
        }
        yield Message::ExplorerStopped;
    }
}

/// The selected key's newest retained sample, read into the pane's facts.
/// `retained()` is oldest-first and covers watched keys only — the pane's
/// caption states that scope.
fn latest_retained(
    monitor: &Monitor,
    key: &str,
    core: &mut ExplorerCore,
) -> Option<InspectedSample> {
    let retained = monitor.core().retained();
    let view = retained.iter().rev().find(|v| v.key == key)?;
    Some(InspectedSample::of(view, core.declared_type(key)))
}
