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
    key_prefix: String,
    serialization: SerializationFormat,
}

impl GnmiSubscriber {
    /// Create a new gNMI subscriber
    pub fn new(target: GnmiTarget, key_prefix: String, serialization: SerializationFormat) -> Self {
        Self {
            target,
            key_prefix,
            serialization,
        }
    }

    /// Run the subscriber, publishing telemetry to Zenoh
    pub async fn run(&self, session: Arc<zenoh::Session>) -> anyhow::Result<()> {
        info!(
            "Starting gNMI subscriber for {} at {}",
            self.target.name, self.target.address
        );

        loop {
            match self.subscribe_loop(&session).await {
                Ok(()) => {
                    info!("Subscription completed normally for {}", self.target.name);
                }
                Err(e) => {
                    error!(
                        "Subscription error for {}: {}. Reconnecting in 5s...",
                        self.target.name, e
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn subscribe_loop(&self, session: &Arc<zenoh::Session>) -> anyhow::Result<()> {
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
                        self.process_notification(session, notification).await?;
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
            sample_interval: sub.sample_interval_ms * 1_000_000, // Convert to nanoseconds
            suppress_redundant: sub.suppress_redundant,
            heartbeat_interval: sub.heartbeat_interval_ms * 1_000_000,
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
        session: &Arc<zenoh::Session>,
        notification: gnmi::Notification,
    ) -> anyhow::Result<()> {
        let timestamp = notification.timestamp / 1_000_000; // Convert nanoseconds to milliseconds
        let prefix_path = notification.prefix.as_ref().map(|p| self.path_to_string(p));

        for update in notification.update {
            if let Some(path) = &update.path {
                let metric_path = self.path_to_string(path);
                let full_path = match &prefix_path {
                    Some(prefix) if !prefix.is_empty() => format!("{}/{}", prefix, metric_path),
                    _ => metric_path,
                };

                let value = self.extract_value(&update);

                let point = TelemetryPoint {
                    timestamp,
                    source: self.target.name.clone(),
                    protocol: Protocol::Gnmi,
                    metric: full_path.clone(),
                    value,
                    labels: HashMap::new(),
                };

                let key = format!("{}/{}/{}", self.key_prefix, self.target.name, full_path);

                let payload = match self.serialization {
                    SerializationFormat::Json => serde_json::to_vec(&point)?,
                    SerializationFormat::Cbor => {
                        let mut buf = Vec::new();
                        ciborium::into_writer(&point, &mut buf)?;
                        buf
                    }
                };

                session
                    .put(&key, payload)
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

    fn extract_value(&self, update: &gnmi::Update) -> TelemetryValue {
        if let Some(val) = &update.val {
            self.typed_value_to_telemetry(val)
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

    fn typed_value_to_telemetry(&self, val: &gnmi::TypedValue) -> TelemetryValue {
        use gnmi::typed_value::Value;

        match &val.value {
            Some(Value::StringVal(s)) => TelemetryValue::Text(s.clone()),
            Some(Value::IntVal(i)) => TelemetryValue::Gauge(*i as f64),
            Some(Value::UintVal(u)) => TelemetryValue::Counter(*u),
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
        };

        let subscriber = GnmiSubscriber::new(
            target,
            "zensight/gnmi".to_string(),
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
        };

        let subscriber = GnmiSubscriber::new(
            target,
            "zensight/gnmi".to_string(),
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
        };

        let subscriber = GnmiSubscriber::new(
            target,
            "zensight/gnmi".to_string(),
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
}
