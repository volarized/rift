//! Model Context Protocol transport boundary.

mod election;
mod failure;
mod http;
pub mod logs;
mod proxy;
mod resource;
pub mod schema;
mod server;
mod spawn;
mod storage;
mod transport;
mod validation;

pub use election::{
    ElectedServer, ElectionError, ElectionFault, ElectionGuard, ServerPresence, StaleReason, claim,
    document_path, probe, read_serving, serve_elected,
};
pub use http::{HttpServeError, HttpServeFault, HttpServer, serve_http};
pub use logs::{
    LOG_QUEUE_RECORDS, LogDrain, LogSink, log_capture, logs_configuration, open_log_store,
};
pub use proxy::{ProxyFault, ProxyServeError, serve_proxy};
pub use server::RiftMcp;
pub use spawn::{
    PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, spawn_detached_server,
};
pub use storage::workspace_database;

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
