//! Hand-authored wire models for the Rift MCP surface.
//!
//! The Rust models in [`read`] are the single source of truth for what the
//! Rift MCP server accepts and returns. The server derives its request and
//! response schemas from these types, and the same derivation is exported
//! into the docs, so the served surface and the documented surface cannot
//! drift apart.

pub mod canonical;
mod retained_size;
pub use retained_size::{
    btree_material_bytes, retained_arc_material, retained_arc_slice_material,
    retained_map_material, retained_sequence_material, retained_value_material,
    retained_vector_material,
};
pub mod configuration;
pub mod dependencies;
pub mod diagnostic;
pub mod documentation;
pub mod error;
pub mod identity;
pub mod index;
pub mod lock;
pub mod map;
pub mod read;
pub mod retry;
pub mod revision;
pub mod schema;
pub mod search;
pub mod search_read;
pub mod source;
pub mod source_read;
pub mod symbol_read;
pub mod workspace;
