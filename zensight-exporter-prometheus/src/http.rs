//! HTTP server for Prometheus metrics endpoint.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::collector::SharedCollector;

/// Application state shared across handlers.
#[derive(Clone)]
struct AppState {
    collector: SharedCollector,
}

/// Create the HTTP router.
fn create_router(collector: SharedCollector, metrics_path: &str) -> Router {
    let state = AppState { collector };

    Router::new()
        .route(metrics_path, get(metrics_handler))
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Handler for the /metrics endpoint.
async fn metrics_handler(State(state): State<AppState>) -> Response {
    let body = state.collector.render();

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Handler for the /health endpoint.
async fn health_handler() -> Response {
    (StatusCode::OK, "healthy\n").into_response()
}

/// Handler for the /ready endpoint.
async fn ready_handler(State(state): State<AppState>) -> Response {
    let stats = state.collector.stats();

    // Consider ready if we've received at least some points
    // or if we've been running for a while (even with no data)
    if stats.points_received > 0 {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "not ready - no telemetry received yet\n",
        )
            .into_response()
    }
}

/// HTTP server configuration.
pub struct HttpServer {
    collector: SharedCollector,
    listen_addr: SocketAddr,
    metrics_path: String,
}

impl HttpServer {
    /// Create a new HTTP server.
    pub fn new(collector: SharedCollector, listen_addr: SocketAddr, metrics_path: String) -> Self {
        Self {
            collector,
            listen_addr,
            metrics_path,
        }
    }

    /// Run the HTTP server until the shutdown signal is received.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        let router = create_router(self.collector, &self.metrics_path);

        info!(
            addr = %self.listen_addr,
            path = %self.metrics_path,
            "Starting HTTP server"
        );

        let listener = tokio::net::TcpListener::bind(self.listen_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind to {}: {}", self.listen_addr, e))?;

        info!(
            addr = %self.listen_addr,
            path = %self.metrics_path,
            "HTTP server listening"
        );

        // Run server with graceful shutdown
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                // Wait for shutdown signal
                loop {
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                    if *shutdown.borrow() {
                        break;
                    }
                }
                info!("HTTP server shutting down");
            })
            .await
            .map_err(|e| anyhow::anyhow!("HTTP server error: {}", e))?;

        info!("HTTP server stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::MetricCollector;
    use crate::config::{AggregationConfig, FilterConfig, PrometheusConfig};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn make_collector() -> SharedCollector {
        Arc::new(MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        ))
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let collector = make_collector();
        let router = create_router(collector, "/metrics");

        let response = router
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().contains("text/plain"));
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let collector = make_collector();
        let router = create_router(collector, "/metrics");

        let response = router
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_ready_endpoint_not_ready() {
        let collector = make_collector();
        let router = create_router(collector, "/metrics");

        let response = router
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();

        // Not ready because no telemetry received
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_ready_endpoint_ready() {
        use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

        let collector = make_collector();

        // Record a point to make it ready
        let point =
            TelemetryPoint::new("test", Protocol::Snmp, "metric", TelemetryValue::Gauge(1.0));
        collector.record(&point);

        let router = create_router(collector, "/metrics");

        let response = router
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_custom_metrics_path() {
        let collector = make_collector();
        let router = create_router(collector, "/prometheus/metrics");

        // Custom path should work
        let response = router
            .clone()
            .oneshot(
                Request::get("/prometheus/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Default path should 404
        let response = router
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
