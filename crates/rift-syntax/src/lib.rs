//! Syntax fact extraction.

mod rust;

pub use rust::{
    ByteRange, RustGrammarNodeKind, RustNode, RustQuery, RustQueryCapture, RustSource, RustSymbol,
    RustSymbolKind, RustSyntaxBound, RustSyntaxDocument, RustSyntaxError, RustSyntaxFault,
    RustSyntaxLimits, RustSyntaxProvider, RustSyntaxViolation, RustVisibility,
};

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
