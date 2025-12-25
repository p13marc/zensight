//! ZenSight - Observability frontend for Zenoh telemetry.
//!
//! This library exposes the core components for testing.

pub mod app;
pub mod message;
pub mod mock;
pub mod subscription;
pub mod view;

// Re-export commonly used types
pub use app::ZenSight;
pub use message::{DeviceId, Message};
