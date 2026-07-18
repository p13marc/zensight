//! Re-export of the v1 keyspace context (lives in `zenkey` so
//! zensight-common can use it too — epic #453).

pub use zenkey::context::{V1Context, host_id};
