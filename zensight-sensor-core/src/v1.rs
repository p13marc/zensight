//! Re-export of the v1 keyspace context (lives in `zenkey` so
//! zensight-common can use it too — epic #453).

pub use zenkey::context::V1Context;
/// The infallible producer→context shim. zenkey 0.7 made
/// `V1Context::for_producer` fallible; the validation happens once, in
/// [`zensight_common::v1`], and every sensor keeps an infallible builder. See
/// that module for why that is honest here.
pub use zensight_common::v1::for_producer;

/// The process-wide host origin, minted once through ZenSight's application
/// profile (RFC 06 §1) — kept here so downstream `v1::host_id()` callers
/// survive the zenkey 0.2 move of the mint into `AppProfile`.
pub fn host_id() -> &'static zenkey::HostId {
    zensight_common::PROFILE.host_id()
}
