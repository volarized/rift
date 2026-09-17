//! Model Context Protocol transport boundary.

mod dependency;
mod election;
mod failure;
mod http;
mod identity;
pub mod logs;
mod parameters;
mod proxy;
mod resource;
pub mod schema;
mod server;
pub mod skill;
mod spawn;
mod storage;
mod transport;
mod validation;

pub use election::{
    ElectedServer, ElectionError, ElectionFault, ElectionGuard, ServerPresence, StaleReason, claim,
    document_path, probe, read_serving, serve_elected, serve_elected_with_storage,
};
pub use http::{HttpServeError, HttpServeFault, HttpServer, TokenCheck, serve_http};
pub use logs::{
    LOG_QUEUE_RECORDS, LogDrain, LogSink, PANIC_PAYLOAD_BYTES_MAX, install_panic_hook, log_capture,
    logs_configuration,
};
pub use proxy::{ProxyFault, ProxyServeError, serve_proxy};
pub use server::RiftMcp;
pub use spawn::{
    BoundedStderr, BoundedWriter, PRESENCE_POLL_INTERVAL, SERVER_STDERR_BYTES_MAX,
    SERVER_STDERR_FILE_NAME, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, SpawnedServer,
    spawn_detached_server, stderr_file_path,
};
pub use storage::WorkspaceStorage;

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
