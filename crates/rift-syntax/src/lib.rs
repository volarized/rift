//! Syntax fact extraction.

mod angular;
mod contribution;
mod css;
mod document;
mod ecmascript;
mod embedded;
mod extract;
mod failure;
mod html;
mod javascript;
mod json;
pub mod language;
mod markdown;
mod native;
mod parse;
mod provider;
mod python;
pub mod registry;
mod rust;
mod tailwind;
mod toml;
mod typescript;
mod web_component;
mod yaml;

pub use angular::{AngularComponent, AngularTemplate, angular_components};
pub use contribution::{
    DocumentPlacement, LogicalDeclaration, PlacedAlias, SYNTAX_PROVIDER_ID,
    SyntaxPublicationBuilder, source_unit, source_unit_for_path,
};
pub use css::CssSyntaxProvider;
pub use document::{
    ByteRange, PythonOverload, RustModulePath, SyntaxDocument, SyntaxExportBinding,
    SyntaxExportKind, SyntaxFacts, SyntaxNode, SyntaxOrigin, SyntaxSymbol,
};
pub use html::HtmlSyntaxProvider;
pub use javascript::JavaScriptSyntaxProvider;
pub use json::JsonSyntaxProvider;
pub use language::{LanguageDefinition, ShippedLanguage, definitions};
pub use markdown::{
    MARKDOWN_INLINE_RANGES_MAX, MARKDOWN_PROGRESS_CALLBACKS_MAX, MarkdownBlockFact,
    MarkdownBlockKind, MarkdownBlockStructure, MarkdownFacts, MarkdownHeadingFact,
    MarkdownLinkFact, MarkdownLinkKind, MarkdownReferenceCandidate, MarkdownSyntaxProvider,
};
pub use provider::{SyntaxBound, SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use python::PythonSyntaxProvider;
pub use rust::{RustQuery, RustQueryCapture, RustSyntaxProvider};
pub use toml::TomlSyntaxProvider;
pub use typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};
pub use web_component::{
    AngularSyntaxProvider, SvelteSyntaxProvider, VueSyntaxProvider, analyze_angular_included,
    append_angular_templates,
};
pub use yaml::YamlSyntaxProvider;

/// Compile-time marker for syntax-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxLayer;

mod restore;
pub use restore::{MarkdownFactsParts, SyntaxFactsParts, SyntaxNames};

pub use tailwind::{TailwindFacts, append_framework_symbols, tailwind_symbols};
