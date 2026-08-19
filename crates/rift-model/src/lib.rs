//! Model acquisition and embedding execution.

/// Compile-time marker for model-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLayer;
