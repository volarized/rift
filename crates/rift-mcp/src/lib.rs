//! Model Context Protocol transport boundary.

mod server;

pub use server::RiftMcp;

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
