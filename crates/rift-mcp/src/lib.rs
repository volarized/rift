//! Model Context Protocol transport boundary.

mod failure;
pub mod schema;
mod server;
mod stdio;
mod validation;

pub use server::RiftMcp;
pub use stdio::{StdioServeError, serve_stdio};

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
