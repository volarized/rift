//! Long-lived workspace application service.

mod read;

pub use read::{ReadError, ReadErrorKind, ReadService};

/// Compile-time marker for server-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLayer;
