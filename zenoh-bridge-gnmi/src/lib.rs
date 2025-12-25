//! gNMI (gRPC Network Management Interface) bridge for Zensight
//!
//! This bridge connects to gNMI-enabled network devices and publishes
//! streaming telemetry to Zenoh.

pub mod config;
pub mod subscriber;

// Include the generated protobuf code
pub mod gnmi_ext {
    tonic::include_proto!("gnmi_ext");
}

pub mod gnmi {
    tonic::include_proto!("gnmi");
}

pub use config::GnmiConfig;
pub use subscriber::GnmiSubscriber;
