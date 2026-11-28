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
mod jvm;
pub mod language;
mod markdown;
mod native;
mod parse;
mod parse_errors;
mod provider;
mod python;
pub mod registry;
mod rust;
mod static_analysis;
mod tailwind;
mod toml;
mod typescript;
mod web_component;
mod yaml;

pub use angular::{AngularComponent, AngularTemplate, angular_components};
pub use contribution::{
    DocumentPlacement, LogicalDeclaration, PackageBindingSlots, PlacedAlias, SYNTAX_PROVIDER_ID,
    ScopedIdentityAnchors, SyntaxPublicationBuilder, source_unit, source_unit_for_path,
};
pub use css::CssSyntaxProvider;
pub use document::{
    ByteRange, MacroInvocation, PythonOverload, RustModulePath, Scope, SyntaxDocument,
    SyntaxElement, SyntaxExportBinding, SyntaxExportKind, SyntaxFacts, SyntaxHolder, SyntaxMatch,
    SyntaxOrigin, SyntaxPlace, SyntaxSymbol,
};
pub use ecmascript::{CheckedInlineTypeImport, CheckedKeyofBound, CheckedPropertySelector};
pub use html::HtmlSyntaxProvider;
pub use javascript::JavaScriptSyntaxProvider;
pub use json::JsonSyntaxProvider;
pub use jvm::{JvmLanguage, JvmOccurrence, JvmOccurrenceKind, JvmOccurrences};
pub use language::{LanguageDefinition, ShippedLanguage, definitions};
pub use markdown::{
    MARKDOWN_INLINE_RANGES_MAX, MARKDOWN_PROGRESS_CALLBACKS_MAX, MarkdownBlockFact,
    MarkdownBlockKind, MarkdownBlockStructure, MarkdownFacts, MarkdownHeadingFact,
    MarkdownLinkFact, MarkdownLinkKind, MarkdownReferenceCandidate, MarkdownSyntaxProvider,
};
pub use provider::{SyntaxBound, SyntaxLimits, SyntaxProvider, SyntaxSource};
pub use python::PythonSyntaxProvider;
pub use rust::{RustQuery, RustQueryCapture, RustSyntaxProvider};
pub use static_analysis::{
    HeritageEdge, PythonDeletion, PythonPosition, StaticAssignedCall, StaticAssignedObject,
    StaticBinding, StaticBindingKind, StaticBodyMember, StaticCall, StaticCallee,
    StaticCommonJsExport, StaticConditionalItem, StaticDeclaredReturn, StaticFacts,
    StaticFactsParts, StaticFunctionContext, StaticInherentImpl, StaticMember, StaticMemberAlias,
    StaticMemberCallback, StaticMemberCopy, StaticParameter, StaticPythonInitializer,
    StaticPythonInitializerField, StaticReexport, StaticReference, StaticRustFieldSite,
    StaticRustGenericReference, StaticRustMacroCall, StaticRustMacroDefinition, StaticRustUse,
    StaticRustValueRead, StaticTypeUnionBranch,
};
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

pub use document::rust_macros::tokens;
