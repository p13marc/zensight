//! Re-export of the v1 keyspace context (lives in `zensight-keyspace` so
//! zensight-common can use it too — epic #453).

pub use zensight_keyspace::context::{V1Context, host_id};
