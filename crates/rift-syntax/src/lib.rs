//! Syntax fact extraction.

mod contribution;
mod document;
mod ecmascript;
mod extract;
mod failure;
mod javascript;
mod json;
pub mod language;
mod markdown;
mod provider;
mod python;
pub mod registry;
mod rust;
mod toml;
mod typescript;
mod yaml;

pub use contribution::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxPublicationBuilder, SyntaxPublicationError,
    source_unit,
};
pub use document::{ByteRange, SyntaxDocument, SyntaxNode, SyntaxSymbol};
pub use failure::{SyntaxBound, SyntaxError, SyntaxFault, SyntaxViolation};
pub use javascript::JavaScriptSyntaxProvider;
pub use json::JsonSyntaxProvider;
pub use language::{LanguageDefinition, ShippedLanguage, definitions};
pub use markdown::MarkdownSyntaxProvider;
pub use provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use python::PythonSyntaxProvider;
pub use rust::{RustCrateLayout, RustQuery, RustQueryCapture, RustSyntaxProvider};
pub use toml::TomlSyntaxProvider;
pub use typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};
pub use yaml::YamlSyntaxProvider;

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;
