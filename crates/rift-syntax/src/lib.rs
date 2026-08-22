//! Syntax fact extraction.

mod rust;

pub use rust::{
    ByteRange, RustGrammarNodeKind, RustNode, RustQuery, RustQueryCapture, RustSource, RustSymbol,
    RustSymbolKind, RustSyntaxBound, RustSyntaxDocument, RustSyntaxError, RustSyntaxFault,
    RustSyntaxLimits, RustSyntaxProvider, RustSyntaxViolation, RustVisibility,
};

/// File extensions some shipped syntax provider parses: the union of every grammar's own
/// [`RustSyntaxProvider::SOURCE_EXTENSIONS`]-style declaration. A new grammar joins the
/// workspace scan by declaring its extensions on its provider and adding them here.
pub const SOURCE_FILE_EXTENSIONS: &[&str] = RustSyntaxProvider::SOURCE_EXTENSIONS;

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
