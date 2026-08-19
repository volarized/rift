//! Syntax fact extraction.

mod rust;

pub use rust::{
    ByteRange, RustNode, RustQuery, RustQueryCapture, RustSource, RustSymbol, RustSymbolKind,
    RustSyntaxDocument, RustSyntaxError, RustSyntaxLimits, RustSyntaxProvider, RustSyntaxViolation,
    RustVisibility,
};

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
