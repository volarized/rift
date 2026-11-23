/// The `PROGRESS` step of an engine that begins and ends its work at start, so the session
/// reads it ready once quiet.
pub(crate) const ANNOUNCED_WORK: &str = r#"frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}'"#;

#[cfg(rift_test_lifecycle_trace)]
mod lifecycle;
#[cfg(rift_test_lifecycle_trace)]
pub(crate) use lifecycle::{LifecycleFixtureReceipt, loading_engine_trace};
