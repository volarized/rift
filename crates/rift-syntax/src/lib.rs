//! Syntax fact extraction.

mod document;
mod ecmascript;
mod extract;
mod failure;
mod javascript;
mod json;
mod markdown;
mod provider;
pub mod registry;
mod rust;
mod typescript;
mod yaml;

pub use document::{ByteRange, SyntaxDocument, SyntaxNode, SyntaxSymbol};
pub use failure::{SyntaxBound, SyntaxError, SyntaxFault, SyntaxViolation};
pub use javascript::JavaScriptSyntaxProvider;
pub use json::JsonSyntaxProvider;
pub use markdown::MarkdownSyntaxProvider;
pub use provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use rust::{RustQuery, RustQueryCapture, RustSyntaxProvider};
pub use typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};
pub use yaml::YamlSyntaxProvider;

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
