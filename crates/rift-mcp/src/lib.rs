//! Model Context Protocol transport boundary.

mod election;
mod failure;
pub use failure::{wire_code_for_error, wire_code_for_slug};
mod global;
mod history;
mod http;
mod identity;
pub mod logs;
mod metrics;
mod output;
mod parameters;
mod proxy;
pub mod repository;
mod repository_http;
mod resource;
pub mod schema;
mod server;
pub mod skill;
mod spawn;
mod storage;
mod transport;
mod validation;

pub use election::{
    ElectedServer, ElectionGuard, ServerPresence, StaleReason, claim, document_path, probe,
    probe_state_directory, read_serving, serve_elected, serve_elected_with_storage,
    serve_repository_elected,
};
pub use failure::{McpErrorExt, McpErrorFailExt, McpFailure};
pub use http::{
    DeferredDatabaseShutdown, HttpServer, STOP_REQUEST_TIMEOUT, StopRequestFailure, TokenCheck,
    request_stop, serve_http, stop_stage,
};
pub use identity::{BuildCheckout, product_identity_of};
pub use logs::logs_configuration;
pub use output::OutputPolicy;
pub use proxy::{forward_budget, serve_proxy};
pub use server::RiftMcp;
pub use spawn::{
    PRESENCE_POLL_INTERVAL, SERVER_STDERR_FILE_NAME, START_POLL_ATTEMPT_COUNT,
    START_SPAWN_COUNT_MAX, START_WAIT_MAX, SpawnPollOutcome, SpawnedServer, StartExit, StartSpawns,
    StartedServer, spawn_detached_server, stderr_file_path,
};
pub use storage::WorkspaceStorage;

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
