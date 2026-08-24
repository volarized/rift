//! Hand-authored wire models for the Rift MCP surface.
//!
//! The Rust models in [`read`] are the single source of truth for what the
//! Rift MCP server accepts and returns. The server derives its request and
//! response schemas from these types, and the same derivation is exported
//! into the docs, so the served surface and the documented surface cannot
//! drift apart.

pub mod change;
pub mod configuration;
pub mod diagnostic;
pub mod error;
pub mod lock;
pub mod read;
pub mod retry;
pub mod schema;
pub mod search;
pub mod source;
