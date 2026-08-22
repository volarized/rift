//! Model Context Protocol transport boundary.

pub mod schema;
mod server;
mod stdio;

pub use server::{RiftMcp, RiftMcpOptions};
pub use stdio::{StdioServeError, serve_stdio};

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
