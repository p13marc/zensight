//! gNMI (gRPC Network Management Interface) bridge for ZenSight
//!
//! This bridge connects to gNMI-enabled network devices and publishes
//! streaming telemetry to Zenoh.

pub mod config;
pub mod subscriber;

// Include the generated protobuf code
#[allow(clippy::doc_lazy_continuation)]
pub mod gnmi_ext {
    tonic::include_proto!("gnmi_ext");
}

#[allow(clippy::doc_lazy_continuation)]
pub mod gnmi {
    tonic::include_proto!("gnmi");
}

pub use config::GnmiConfig;
pub use subscriber::GnmiSubscriber;
