//! Netlink polling collector: reads kernel state via nlink and publishes
//! telemetry.

use std::sync::Arc;
use std::time::Duration;

use nlink::netlink::{Connection, Route, SockDiag};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};
use zensight_common::{Format, encode};

use crate::config::NetlinkConfig;
use crate::map::{self, IfaceSample, SocketCounts};

/// Polls netlink on an interval and publishes interface + socket telemetry.
pub struct Collector {
    host: String,
    key_prefix: String,
    config: NetlinkConfig,
    session: Arc<zenoh::Session>,
    format: Format,
}

impl Collector {
    pub fn new(
        host: String,
        config: NetlinkConfig,
        session: Arc<zenoh::Session>,
        format: Format,
    ) -> Self {
        Self {
            host,
            key_prefix: config.key_prefix.clone(),
            config,
            session,
            format,
        }
    }

    pub async fn run(self) {
        let route = match Connection::<Route>::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to open netlink route connection");
                return;
            }
        };
        let sockdiag = Connection::<SockDiag>::new().ok();
        if self.config.collect.sockets && sockdiag.is_none() {
            tracing::warn!("sockdiag unavailable; socket telemetry disabled");
        }

        let mut tick = tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs));
        loop {
            tick.tick().await;
            if self.config.collect.interfaces {
                self.poll_interfaces(&route).await;
            }
            if self.config.collect.sockets
                && let Some(sd) = &sockdiag
            {
                self.poll_sockets(sd).await;
            }
        }
    }

    async fn poll_interfaces(&self, conn: &Connection<Route>) {
        let links = match conn.get_links().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "get_links failed");
                return;
            }
        };
        for link in links {
            let name = link.name_or("?").to_string();
            if name == "?" || !self.config.interfaces.should_include(&name) {
                continue;
            }
            let stats = link.stats();
            let sample = IfaceSample {
                name: name.clone(),
                ifindex: link.ifindex(),
                up: link.is_up(),
                carrier: link.carrier(),
                mtu: link.mtu(),
                mac: link.mac_address(),
                oper_state: link.operstate().map(|o| format!("{o:?}").to_lowercase()),
                rx_bytes: stats.map(|s| s.rx_bytes).unwrap_or(0),
                tx_bytes: stats.map(|s| s.tx_bytes).unwrap_or(0),
                rx_packets: stats.map(|s| s.rx_packets).unwrap_or(0),
                tx_packets: stats.map(|s| s.tx_packets).unwrap_or(0),
                rx_errors: stats.map(|s| s.rx_errors).unwrap_or(0),
                tx_errors: stats.map(|s| s.tx_errors).unwrap_or(0),
                rx_dropped: stats.map(|s| s.rx_dropped).unwrap_or(0),
                tx_dropped: stats.map(|s| s.tx_dropped).unwrap_or(0),
            };
            for point in map::iface_points(&self.host, &sample) {
                self.publish(&point).await;
            }
        }
    }

    async fn poll_sockets(&self, conn: &Connection<SockDiag>) {
        let filter = SocketFilter::tcp().all_states().with_tcp_info().build();
        let socks = match conn.query(&filter).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "sockdiag query failed");
                return;
            }
        };
        let counts = aggregate_sockets(&socks);
        for point in map::socket_points(&self.host, &counts) {
            self.publish(&point).await;
        }
    }

    async fn publish(&self, point: &zensight_common::TelemetryPoint) {
        let key = format!("{}/{}/{}", self.key_prefix, point.source, point.metric);
        match encode(point, self.format) {
            Ok(payload) => {
                if let Err(e) = self.session.put(&key, payload).await {
                    tracing::warn!(error = %e, key = %key, "publish failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "encode failed"),
        }
    }
}

/// Aggregate a set of sockdiag results into [`SocketCounts`]. Pure; unit-tested.
pub fn aggregate_sockets(socks: &[SocketInfo]) -> SocketCounts {
    let mut c = SocketCounts::default();
    for s in socks {
        let SocketInfo::Inet(inet) = s else { continue };
        match inet.state {
            SocketState::Tcp(TcpState::Established) | SocketState::Established => {
                c.established += 1
            }
            SocketState::Tcp(TcpState::Listen) | SocketState::Listen => c.listen += 1,
            SocketState::Tcp(TcpState::TimeWait) => c.time_wait += 1,
            SocketState::Tcp(TcpState::SynSent) => c.syn_sent += 1,
            SocketState::Tcp(TcpState::CloseWait) => c.close_wait += 1,
            _ => {}
        }
        if let Some(ti) = &inet.tcp_info {
            c.retransmits_total += ti.retrans as u64;
            c.max_rtt_us = c.max_rtt_us.max(ti.rtt as u64);
        }
    }
    c
}
