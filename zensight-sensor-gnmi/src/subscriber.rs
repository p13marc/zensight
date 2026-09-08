//! gNMI subscription client

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tonic::Request;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::{debug, error, info, warn};

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

use crate::config::{GnmiTarget, SerializationFormat, Subscription, SubscriptionMode};
use crate::gnmi::{
    self, Path, PathElem, SubscribeRequest, SubscriptionList, SubscriptionMode as ProtoSubMode,
    g_nmi_client::GNmiClient,
};

/// A gNMI subscriber that connects to a target and streams telemetry
pub struct GnmiSubscriber {
    target: GnmiTarget,
    /// The v1 telemetry prefix keys hang off.
    telemetry_prefix: String,
    serialization: SerializationFormat,
    /// Set by [`GnmiSubscriber::with_thresholds`] (#931); installed on the
    /// registry this subscriber builds in `run`.
    thresholds: Option<Arc<dyn zensight_common::point_observer::PointObserver>>,
}

impl GnmiSubscriber {
    /// Create a new gNMI subscriber
    pub fn new(
        target: GnmiTarget,
        telemetry_prefix: String,
        serialization: SerializationFormat,
    ) -> Self {
        Self {
            target,
            telemetry_prefix,
            serialization,
            thresholds: None,
        }
    }

    /// The operator's threshold evaluator (#931).
    ///
    /// It has to arrive here rather than in `main.rs`: each subscriber builds
    /// its own registry inside `run`, one per target, and every point this
    /// sensor publishes rides one of them.
    pub fn with_thresholds(
        mut self,
        observer: Arc<dyn zensight_common::point_observer::PointObserver>,
    ) -> Self {
        self.thresholds = Some(observer);
        self
    }

    /// Run the subscriber, publishing telemetry to Zenoh
    pub async fn run(&self, session: Arc<zenoh::Session>) -> anyhow::Result<()> {
        info!(
            "Starting gNMI subscriber for {} at {}",
            self.target.name, self.target.address
        );

        // Telemetry goes through declared publishers (declare-on-first-use + cache
        // per key, drop QoS), never a one-shot `session.put`.
        let registry = zensight_common::PublisherRegistry::new(session);
        if let Some(observer) = &self.thresholds {
            registry.set_observer(observer.clone());
        }

        let mut backoff = Duration::from_secs(5);
        let max_backoff = Duration::from_secs(300);
        let mut attempt = 0u64;

        loop {
            attempt += 1;
            info!(
                attempt,
                backoff_secs = backoff.as_secs(),
                target = %self.target.name,
                "Connecting to gNMI target"
            );

            match self.subscribe_loop(&registry).await {
                Ok(()) => {
                    info!("Subscription completed normally for {}", self.target.name);
                    // Reset on successful connection
                    backoff = Duration::from_secs(5);
                    attempt = 0;
                }
                Err(e) => {
                    warn!(
                        attempt,
                        error = %e,
                        next_retry_secs = backoff.as_secs(),
                        target = %self.target.name,
                        "gNMI connection failed"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
            tokio::time::sleep(backoff).await;
        }
    }

    // The gNMI proto marks `FloatVal`, `DecimalVal` and `Response::Error` as
    // deprecated (prost 0.14 forwards the annotation; 0.13 did not). Devices in
    // the field still send them, and a collector that stopped decoding a value
    // a switch emits would be the regression — the arms stay.
    #[allow(deprecated)]
    async fn subscribe_loop(
        &self,
        registry: &zensight_common::PublisherRegistry,
    ) -> anyhow::Result<()> {
        let channel = self.connect().await?;
        let mut client = GNmiClient::new(channel);

        // Add authentication metadata if configured
        let subscribe_request = self.build_subscribe_request();

        let request = if let Some(ref creds) = self.target.credentials {
            let mut req = Request::new(tokio_stream::once(subscribe_request));
            req.metadata_mut()
                .insert("username", creds.username.parse()?);
            req.metadata_mut()
                .insert("password", creds.password.parse()?);
            req
        } else {
            Request::new(tokio_stream::once(subscribe_request))
        };

        let response = client.subscribe(request).await?;
        let mut stream = response.into_inner();

        info!("gNMI subscription established for {}", self.target.name);

        while let Some(msg) = stream.message().await? {
            if let Some(response) = msg.response {
                match response {
                    gnmi::subscribe_response::Response::Update(notification) => {
                        self.process_notification(registry, notification).await?;
                    }
                    gnmi::subscribe_response::Response::SyncResponse(sync) => {
                        debug!("Received sync response: {}", sync);
                    }
                    gnmi::subscribe_response::Response::Error(err) => {
                        error!("Received gNMI error: {:?}", err);
                    }
                }
            }
        }

        Ok(())
    }

    async fn connect(&self) -> anyhow::Result<Channel> {
        let scheme = if self.target.tls.enabled {
            "https"
        } else {
            "http"
        };
        let uri = format!("{}://{}", scheme, self.target.address);

        let mut endpoint = Endpoint::from_shared(uri)?;

        if self.target.tls.enabled {
            let mut tls_config = ClientTlsConfig::new();

            if self.target.tls.skip_verify {
                // Note: In production, this should use rustls with proper cert handling
                warn!("TLS verification disabled - not recommended for production");
            }

            if let Some(ref ca_cert_path) = self.target.tls.ca_cert {
                let ca_cert = tokio::fs::read(ca_cert_path).await?;
                let ca_cert = tonic::transport::Certificate::from_pem(ca_cert);
                tls_config = tls_config.ca_certificate(ca_cert);
            }

            if let (Some(cert_path), Some(key_path)) =
                (&self.target.tls.client_cert, &self.target.tls.client_key)
            {
                let cert = tokio::fs::read(cert_path).await?;
                let key = tokio::fs::read(key_path).await?;
                let identity = tonic::transport::Identity::from_pem(cert, key);
                tls_config = tls_config.identity(identity);
            }

            endpoint = endpoint.tls_config(tls_config)?;
        }

        let channel = endpoint.connect().await?;
        Ok(channel)
    }

    fn build_subscribe_request(&self) -> SubscribeRequest {
        let subscriptions: Vec<gnmi::Subscription> = self
            .target
            .subscriptions
            .iter()
            .map(|sub| self.build_subscription(sub))
            .collect();

        let subscription_list = SubscriptionList {
            prefix: None,
            subscription: subscriptions,
            mode: gnmi::subscription_list::Mode::Stream as i32,
            encoding: self.target.encoding.to_proto(),
            ..Default::default()
        };

        SubscribeRequest {
            request: Some(gnmi::subscribe_request::Request::Subscribe(
                subscription_list,
            )),
            extension: vec![],
        }
    }

    fn build_subscription(&self, sub: &Subscription) -> gnmi::Subscription {
        let path = self.parse_path(&sub.path);
        let mode = match sub.mode {
            SubscriptionMode::OnChange => ProtoSubMode::OnChange as i32,
            SubscriptionMode::Sample => ProtoSubMode::Sample as i32,
            SubscriptionMode::TargetDefined => ProtoSubMode::TargetDefined as i32,
        };

        gnmi::Subscription {
            path: Some(path),
            mode,
            sample_interval: sub.sample_interval_ms.saturating_mul(1_000_000),
            suppress_redundant: sub.suppress_redundant,
            heartbeat_interval: sub.heartbeat_interval_ms.saturating_mul(1_000_000),
        }
    }

    fn parse_path(&self, path_str: &str) -> Path {
        // Parse XPath-like path into gNMI Path
        // e.g., "/interfaces/interface[name=eth0]/state/counters"
        let mut elems = Vec::new();

        for segment in path_str.split('/').filter(|s| !s.is_empty()) {
            if let Some((name, keys)) = Self::parse_path_segment(segment) {
                elems.push(PathElem { name, key: keys });
            }
        }

        Path {
            elem: elems,
            ..Default::default()
        }
    }

    fn parse_path_segment(segment: &str) -> Option<(String, HashMap<String, String>)> {
        // Parse "interface[name=eth0]" into ("interface", {"name": "eth0"})
        if let Some(bracket_pos) = segment.find('[') {
            let name = segment[..bracket_pos].to_string();
            let keys_str = &segment[bracket_pos + 1..segment.len() - 1];
            let mut keys = HashMap::new();

            for key_val in keys_str.split(',') {
                if let Some((k, v)) = key_val.split_once('=') {
                    keys.insert(k.trim().to_string(), v.trim().to_string());
                }
            }

            Some((name, keys))
        } else {
            Some((segment.to_string(), HashMap::new()))
        }
    }

    async fn process_notification(
        &self,
        registry: &zensight_common::PublisherRegistry,
        notification: gnmi::Notification,
    ) -> anyhow::Result<()> {
        // The device's own clock, clamped to this host's (#1077).
        //
        // `checked_div(1_000_000).unwrap_or(0)` was dead code twice over: the
        // divisor is a nonzero constant so `checked_div` never returns `None`,
        // and the case it looked like it was guarding — `timestamp == 0`, which
        // the gNMI spec defines as **unset** — fell straight through and
        // published the point at the epoch. A switch that has not reached NTP
        // after a reload, which is the common case, published months out.
        let received_ms = zensight_common::current_timestamp_millis();
        let timestamp = self.clamp_timestamp(notification.timestamp, received_ms);
        let prefix_path = notification.prefix.as_ref().map(|p| self.path_to_string(p));

        for update in notification.update {
            if let Some(path) = &update.path {
                let metric_path = self.path_to_string(path);
                let full_path = match &prefix_path {
                    Some(prefix) if !prefix.is_empty() => format!("{}/{}", prefix, metric_path),
                    _ => metric_path,
                };

                let value = self.extract_value(&update, &full_path);

                let point = TelemetryPoint {
                    timestamp,
                    source: self.target.name.clone(),
                    protocol: Protocol::Gnmi,
                    metric: full_path.clone(),
                    value,
                    labels: HashMap::new(),
                    unit: None,
                };

                let key = format!(
                    "{}/{}/{}",
                    self.telemetry_prefix, self.target.name, full_path
                );

                // `put_point`, not a hand-rolled encode: the last place this
                // is a `TelemetryPoint` rather than bytes, and where the
                // operator's threshold rules see it (#931).
                registry
                    .put_point(
                        &key,
                        &point,
                        zensight_common::QosClass::Telemetry,
                        self.serialization.into(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("Zenoh put failed: {}", e))?;
                debug!("Published telemetry to {}", key);
            }
        }

        Ok(())
    }

    fn path_to_string(&self, path: &Path) -> String {
        path.elem
            .iter()
            .map(|elem| {
                if elem.key.is_empty() {
                    elem.name.clone()
                } else {
                    let keys: Vec<String> = elem
                        .key
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, v))
                        .collect();
                    format!("{}[{}]", elem.name, keys.join(","))
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The instant to publish a notification at (#1077).
    ///
    /// gNMI carries nanoseconds since the epoch, and the device's clock is not
    /// this host's. Three cases, and only the first was handled:
    ///
    /// - a plausible device clock → use it, so points from one device order
    ///   among themselves the way the device saw them;
    /// - `0`, which the spec defines as **unset** → receive time. It used to
    ///   publish at 1970-01-01, and the `unwrap_or(0)` that looked like it was
    ///   guarding this was dead code: `checked_div` by a nonzero constant never
    ///   returns `None`;
    /// - beyond `max_clock_skew_secs` → receive time, with the skew logged. A
    ///   switch that has not reached NTP after a reload is the common case, and
    ///   it publishes months out.
    fn clamp_timestamp(&self, device_nanos: i64, received_ms: i64) -> i64 {
        if device_nanos <= 0 {
            tracing::debug!(
                target_name = %self.target.name,
                "gNMI notification carried no timestamp; using receive time"
            );
            return received_ms;
        }
        let device_ms = device_nanos / 1_000_000;
        let skew_secs = self.target.max_clock_skew_secs;
        if skew_secs == 0 {
            return device_ms; // the clamp is off; trust the device
        }
        let limit_ms = (skew_secs as i64).saturating_mul(1_000);
        if (device_ms - received_ms).abs() > limit_ms {
            tracing::warn!(
                target_name = %self.target.name,
                device_ms, received_ms,
                skew_secs = (device_ms - received_ms) / 1_000,
                "gNMI device clock is outside max_clock_skew_secs; publishing at receive time"
            );
            return received_ms;
        }
        device_ms
    }

    fn extract_value(&self, update: &gnmi::Update, path: &str) -> TelemetryValue {
        if let Some(val) = &update.val {
            self.typed_value_to_telemetry(val, path)
        } else {
            #[allow(deprecated)]
            if let Some(val) = &update.value {
                // Deprecated field, but some implementations still use it
                TelemetryValue::Binary(val.value.clone())
            } else {
                TelemetryValue::Text("".to_string())
            }
        }
    }

    // The gNMI proto marks `FloatVal`, `DecimalVal` and `Response::Error` as
    // deprecated (prost 0.14 forwards the annotation; 0.13 did not). Devices in
    // the field still send them, and a collector that stopped decoding a value
    // a switch emits would be the regression — the arms stay.
    #[allow(deprecated)]
    fn typed_value_to_telemetry(&self, val: &gnmi::TypedValue, path: &str) -> TelemetryValue {
        use gnmi::typed_value::Value;

        match &val.value {
            Some(Value::StringVal(s)) => TelemetryValue::Text(s.clone()),
            Some(Value::IntVal(i)) => TelemetryValue::Gauge(*i as f64),
            // The PATH decides, not the wire type (#1077). OpenConfig uses
            // `uint64` for `state/counters/in-octets` — a counter — and for
            // `cpu/utilization/state/instant`, `temperature/instant` and
            // `memory/state/used` — all levels. Publishing every `UintVal` as a
            // `Counter` made a backend `rate()` CPU utilisation and read every
            // legitimate decrease as a reset.
            Some(Value::UintVal(u)) => match self.target.kind_for_path(path) {
                crate::config::ValueKind::Counter => TelemetryValue::Counter(*u),
                crate::config::ValueKind::Gauge => TelemetryValue::Gauge(*u as f64),
            },
            Some(Value::BoolVal(b)) => TelemetryValue::Boolean(*b),
            Some(Value::BytesVal(b)) => TelemetryValue::Binary(b.clone()),
            Some(Value::FloatVal(f)) => TelemetryValue::Gauge(*f as f64),
            Some(Value::DoubleVal(d)) => TelemetryValue::Gauge(*d),
            Some(Value::DecimalVal(d)) => {
                TelemetryValue::Gauge(d.digits as f64 * 10f64.powi(-(d.precision as i32)))
            }
            Some(Value::LeaflistVal(ll)) => {
                // Convert leaf list to JSON array string
                let values: Vec<String> = ll
                    .element
                    .iter()
                    .map(|e| self.typed_value_to_string(e))
                    .collect();
                TelemetryValue::Text(format!("[{}]", values.join(",")))
            }
            Some(Value::AnyVal(any)) => TelemetryValue::Binary(any.value.clone()),
            Some(Value::JsonVal(j)) => TelemetryValue::Text(String::from_utf8_lossy(j).to_string()),
            Some(Value::JsonIetfVal(j)) => {
                TelemetryValue::Text(String::from_utf8_lossy(j).to_string())
            }
            Some(Value::AsciiVal(a)) => TelemetryValue::Text(a.clone()),
            Some(Value::ProtoBytes(p)) => TelemetryValue::Binary(p.clone()),
            None => TelemetryValue::Text("".to_string()),
        }
    }

    // The gNMI proto marks `FloatVal`, `DecimalVal` and `Response::Error` as
    // deprecated (prost 0.14 forwards the annotation; 0.13 did not). Devices in
    // the field still send them, and a collector that stopped decoding a value
    // a switch emits would be the regression — the arms stay.
    #[allow(deprecated)]
    fn typed_value_to_string(&self, val: &gnmi::TypedValue) -> String {
        use gnmi::typed_value::Value;

        match &val.value {
            Some(Value::StringVal(s)) => format!("\"{}\"", s),
            Some(Value::IntVal(i)) => i.to_string(),
            Some(Value::UintVal(u)) => u.to_string(),
            Some(Value::BoolVal(b)) => b.to_string(),
            Some(Value::FloatVal(f)) => f.to_string(),
            Some(Value::DoubleVal(d)) => d.to_string(),
            _ => "null".to_string(),
        }
    }
}

#[cfg(test)]
#[allow(deprecated)] // the fixtures build the same legacy variants the decoder must accept
mod tests {
    use super::*;
    use crate::config::GnmiEncoding;

    #[test]
    fn test_parse_path_simple() {
        let target = GnmiTarget {
            name: "test".to_string(),
            address: "localhost:9339".to_string(),
            credentials: None,
            tls: Default::default(),
            subscriptions: vec![],
            encoding: GnmiEncoding::Json,
            counter_paths: Vec::new(),
            gauge_paths: Vec::new(),
            max_clock_skew_secs: 300,
        };

        let subscriber = GnmiSubscriber::new(
            target,
            zensight_sensor_core::v1::for_producer("gnmi")
                .telemetry_prefix()
                .into(),
            SerializationFormat::Json,
        );

        let path = subscriber.parse_path("/interfaces/interface/state");
        assert_eq!(path.elem.len(), 3);
        assert_eq!(path.elem[0].name, "interfaces");
        assert_eq!(path.elem[1].name, "interface");
        assert_eq!(path.elem[2].name, "state");
    }

    #[test]
    fn test_parse_path_with_keys() {
        let target = GnmiTarget {
            name: "test".to_string(),
            address: "localhost:9339".to_string(),
            credentials: None,
            tls: Default::default(),
            subscriptions: vec![],
            encoding: GnmiEncoding::Json,
            counter_paths: Vec::new(),
            gauge_paths: Vec::new(),
            max_clock_skew_secs: 300,
        };

        let subscriber = GnmiSubscriber::new(
            target,
            zensight_sensor_core::v1::for_producer("gnmi")
                .telemetry_prefix()
                .into(),
            SerializationFormat::Json,
        );

        let path = subscriber.parse_path("/interfaces/interface[name=eth0]/state");
        assert_eq!(path.elem.len(), 3);
        assert_eq!(path.elem[1].name, "interface");
        assert_eq!(path.elem[1].key.get("name"), Some(&"eth0".to_string()));
    }

    #[test]
    fn test_path_to_string() {
        let target = GnmiTarget {
            name: "test".to_string(),
            address: "localhost:9339".to_string(),
            credentials: None,
            tls: Default::default(),
            subscriptions: vec![],
            encoding: GnmiEncoding::Json,
            counter_paths: Vec::new(),
            gauge_paths: Vec::new(),
            max_clock_skew_secs: 300,
        };

        let subscriber = GnmiSubscriber::new(
            target,
            zensight_sensor_core::v1::for_producer("gnmi")
                .telemetry_prefix()
                .into(),
            SerializationFormat::Json,
        );

        let mut path = Path::default();
        path.elem.push(PathElem {
            name: "interfaces".to_string(),
            key: HashMap::new(),
        });
        path.elem.push(PathElem {
            name: "interface".to_string(),
            key: [("name".to_string(), "eth0".to_string())]
                .into_iter()
                .collect(),
        });

        let result = subscriber.path_to_string(&path);
        assert_eq!(result, "interfaces/interface[name=eth0]");
    }

    #[test]
    fn test_parse_path_segment() {
        let (name, keys) = GnmiSubscriber::parse_path_segment("interface[name=eth0]").unwrap();
        assert_eq!(name, "interface");
        assert_eq!(keys.get("name"), Some(&"eth0".to_string()));

        let (name, keys) = GnmiSubscriber::parse_path_segment("state").unwrap();
        assert_eq!(name, "state");
        assert!(keys.is_empty());
    }

    fn test_subscriber() -> GnmiSubscriber {
        let target = GnmiTarget {
            name: "test".to_string(),
            address: "localhost:9339".to_string(),
            credentials: None,
            tls: Default::default(),
            subscriptions: vec![],
            encoding: GnmiEncoding::Json,
            counter_paths: Vec::new(),
            gauge_paths: Vec::new(),
            max_clock_skew_secs: 300,
        };
        GnmiSubscriber::new(
            target,
            zensight_sensor_core::v1::for_producer("gnmi")
                .telemetry_prefix()
                .into(),
            SerializationFormat::Json,
        )
    }

    fn typed(value: gnmi::typed_value::Value) -> gnmi::TypedValue {
        gnmi::TypedValue { value: Some(value) }
    }

    #[test]
    fn test_parse_path_segment_multiple_keys() {
        // Multiple keys are comma-separated inside a single bracket group.
        let (name, keys) =
            GnmiSubscriber::parse_path_segment("interface[name=eth0,index=0]").unwrap();
        assert_eq!(name, "interface");
        assert_eq!(keys.get("name"), Some(&"eth0".to_string()));
        assert_eq!(keys.get("index"), Some(&"0".to_string()));
    }

    #[test]
    fn test_parse_path_segment_empty() {
        // An empty segment has no '[', so it parses to an empty-named, key-less
        // segment (not None).
        let (name, keys) = GnmiSubscriber::parse_path_segment("").unwrap();
        assert_eq!(name, "");
        assert!(keys.is_empty());
    }

    #[test]
    fn test_typed_value_to_telemetry_scalars() {
        use gnmi::typed_value::Value;
        let sub = test_subscriber();

        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::StringVal("hi".to_string())), "some/leaf"),
            TelemetryValue::Text("hi".to_string())
        );
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::IntVal(-5)), "some/leaf"),
            TelemetryValue::Gauge(-5.0)
        );
        // A `uint64` under no `/counters/` segment is a LEVEL (#1077). This
        // assertion used to read `Counter(42)` for every path, which is the bug:
        // OpenConfig types `cpu/utilization/state/instant` as `uint64` too.
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::UintVal(42)), "some/leaf"),
            TelemetryValue::Gauge(42.0)
        );
        assert_eq!(
            sub.typed_value_to_telemetry(
                &typed(Value::UintVal(42)),
                "interfaces/interface/state/counters/in-octets"
            ),
            TelemetryValue::Counter(42)
        );
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::BoolVal(true)), "some/leaf"),
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::DoubleVal(1.5)), "some/leaf"),
            TelemetryValue::Gauge(1.5)
        );
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::BytesVal(vec![1, 2, 3])), "some/leaf"),
            TelemetryValue::Binary(vec![1, 2, 3])
        );
    }

    /// The path decides whether a `uint64` is a counter (#1077).
    ///
    /// OpenConfig uses `uint64` for `state/counters/in-octets` — a counter —
    /// and for `cpu/utilization/state/instant`, `temperature/instant` and
    /// `memory/state/used`, all of which are levels. Publishing every `UintVal`
    /// as a `Counter` makes a backend `rate()` CPU utilisation, and every
    /// legitimate decrease look like a reset.
    #[test]
    fn the_path_decides_whether_a_uint_is_a_counter() {
        use crate::config::ValueKind;
        let sub = test_subscriber();
        for counter in [
            "interfaces/interface[name=eth0]/state/counters/in-octets",
            "interfaces/interface/state/counters/out-errors",
            "counters/in-pkts",
        ] {
            assert_eq!(
                sub.target.kind_for_path(counter),
                ValueKind::Counter,
                "{counter}"
            );
        }
        for gauge in [
            "components/component/cpu/utilization/state/instant",
            "components/component/state/temperature/instant",
            "system/memory/state/used",
            "interfaces/interface/state/mtu",
        ] {
            assert_eq!(sub.target.kind_for_path(gauge), ValueKind::Gauge, "{gauge}");
        }
    }

    /// A vendor tree that puts counters somewhere else, and one that puts a
    /// level under `/counters/`, are both config — and the gauge list wins,
    /// because a level typed as a counter is the failure that matters.
    #[test]
    fn the_kind_map_overrides_the_path_convention() {
        use crate::config::ValueKind;
        let mut sub = test_subscriber();
        sub.target.counter_paths = vec!["/stats/total-".into()];
        sub.target.gauge_paths = vec!["/counters/queue-depth".into()];
        assert_eq!(
            sub.target.kind_for_path("vendor/stats/total-drops"),
            ValueKind::Counter
        );
        assert_eq!(
            sub.target.kind_for_path("qos/state/counters/queue-depth"),
            ValueKind::Gauge,
            "the gauge list wins over the /counters/ convention"
        );
    }

    /// A device clock is used when it is plausible, and not otherwise (#1077).
    ///
    /// `checked_div(1_000_000).unwrap_or(0)` was dead code twice: the divisor
    /// is a nonzero constant so it never returns `None`, and the case it looked
    /// like it guarded — `timestamp == 0`, which the spec defines as *unset* —
    /// fell through and published the point at the epoch. A switch that has not
    /// reached NTP after a reload published months out.
    #[test]
    fn a_device_clock_is_used_only_while_it_is_plausible() {
        let sub = test_subscriber(); // max_clock_skew_secs: 300
        let now_ms = 1_757_325_600_000i64;
        let nanos = |ms: i64| ms * 1_000_000;

        // In step: the device's own clock, so points from one device order the
        // way the device saw them.
        assert_eq!(
            sub.clamp_timestamp(nanos(now_ms - 1_000), now_ms),
            now_ms - 1_000
        );
        // Unset — the case that used to publish at 1970-01-01.
        assert_eq!(sub.clamp_timestamp(0, now_ms), now_ms);
        assert_eq!(sub.clamp_timestamp(-1, now_ms), now_ms);
        // Months out, in either direction: receive time.
        assert_eq!(
            sub.clamp_timestamp(nanos(now_ms - 90 * 86_400_000), now_ms),
            now_ms
        );
        assert_eq!(
            sub.clamp_timestamp(nanos(now_ms + 90 * 86_400_000), now_ms),
            now_ms
        );
        // Just inside the window is still the device's.
        let inside = now_ms - 299_000;
        assert_eq!(sub.clamp_timestamp(nanos(inside), now_ms), inside);

        // A zero skew disables the clamp — the pre-#1077 behaviour, kept
        // reachable for an operator who wants it.
        let mut trusting = test_subscriber();
        trusting.target.max_clock_skew_secs = 0;
        let months_ago = now_ms - 90 * 86_400_000;
        assert_eq!(
            trusting.clamp_timestamp(nanos(months_ago), now_ms),
            months_ago
        );
        // …but `unset` is still unset. There is no timestamp to trust.
        assert_eq!(trusting.clamp_timestamp(0, now_ms), now_ms);
    }

    #[test]
    fn test_typed_value_to_telemetry_json_and_leaflist() {
        use gnmi::typed_value::Value;
        let sub = test_subscriber();

        assert_eq!(
            sub.typed_value_to_telemetry(
                &typed(Value::JsonVal(b"{\"a\":1}".to_vec())),
                "some/leaf"
            ),
            TelemetryValue::Text("{\"a\":1}".to_string())
        );

        let ll = gnmi::ScalarArray {
            element: vec![
                typed(Value::IntVal(1)),
                typed(Value::StringVal("x".to_string())),
            ],
        };
        assert_eq!(
            sub.typed_value_to_telemetry(&typed(Value::LeaflistVal(ll)), "some/leaf"),
            TelemetryValue::Text("[1,\"x\"]".to_string())
        );
    }

    #[test]
    fn test_typed_value_to_telemetry_none_is_empty_text() {
        let sub = test_subscriber();
        assert_eq!(
            sub.typed_value_to_telemetry(&gnmi::TypedValue { value: None }, "some/leaf"),
            TelemetryValue::Text(String::new())
        );
    }

    #[test]
    fn test_typed_value_to_string_quoting() {
        use gnmi::typed_value::Value;
        let sub = test_subscriber();

        assert_eq!(
            sub.typed_value_to_string(&typed(Value::StringVal("s".to_string()))),
            "\"s\""
        );
        assert_eq!(sub.typed_value_to_string(&typed(Value::IntVal(7))), "7");
        assert_eq!(
            sub.typed_value_to_string(&typed(Value::BoolVal(false))),
            "false"
        );
        // Unhandled variants fall through to "null".
        assert_eq!(
            sub.typed_value_to_string(&typed(Value::BytesVal(vec![0]))),
            "null"
        );
    }

    #[test]
    fn test_extract_value_prefers_val_over_deprecated() {
        use gnmi::typed_value::Value;
        let sub = test_subscriber();

        let update = gnmi::Update {
            val: Some(typed(Value::UintVal(99))),
            ..Default::default()
        };
        assert_eq!(
            sub.extract_value(&update, "interfaces/interface/state/counters/in-octets"),
            TelemetryValue::Counter(99)
        );

        // No value at all -> empty text.
        let empty = gnmi::Update::default();
        assert_eq!(
            sub.extract_value(&empty, "some/leaf"),
            TelemetryValue::Text(String::new())
        );
    }
}
