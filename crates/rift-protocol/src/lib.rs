//! Hand-authored protocol models and exported schemas for Rift.
//!
//! The Rust models in this crate are the single source of truth for the Rift
//! MCP wire surface. [`schema::schema_document`] renders the JSON Schema
//! document the docs site publishes, and the `rift-schema-export` binary
//! writes it to disk.

pub mod contracts;
pub mod read;
pub mod schema;
