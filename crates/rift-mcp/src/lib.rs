//! Model Context Protocol transport boundary.

pub mod schema;
mod server;

pub use server::RiftMcp;

/// Compile-time marker for MCP-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLayer;
