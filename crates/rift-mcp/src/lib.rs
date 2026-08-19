//! Model Context Protocol transport boundary.

mod server;
mod stdio;

pub use server::RiftMcp;
pub use stdio::{StdioServeError, serve_stdio};

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
