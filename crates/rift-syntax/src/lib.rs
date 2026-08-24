//! Syntax fact extraction.

mod document;
mod extract;
mod failure;
mod provider;
pub mod registry;
mod rust;

pub use document::{ByteRange, SyntaxDocument, SyntaxNode, SyntaxSymbol};
pub use failure::{SyntaxBound, SyntaxError, SyntaxFault, SyntaxViolation};
pub use provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use rust::{RustQuery, RustQueryCapture, RustSyntaxProvider};

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
