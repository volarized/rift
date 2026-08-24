//! Syntax fact extraction.

mod document;
mod ecmascript;
mod extract;
mod failure;
mod javascript;
mod markdown;
mod provider;
pub mod registry;
mod rust;
mod typescript;

pub use document::{ByteRange, SyntaxDocument, SyntaxNode, SyntaxSymbol};
pub use failure::{SyntaxBound, SyntaxError, SyntaxFault, SyntaxViolation};
pub use javascript::JavaScriptSyntaxProvider;
pub use markdown::MarkdownSyntaxProvider;
pub use provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use rust::{RustQuery, RustQueryCapture, RustSyntaxProvider};
pub use typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
