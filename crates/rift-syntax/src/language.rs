//! The languages this build ships: one closed identity, one definition list.
//!
//! A new language is one [`LanguageDefinition`] implementation and one entry
//! in [`definitions`]. The registry, the workspace walk, and the effective
//! language policy all derive from that list, so nothing else names a
//! language twice.

use rift_protocol::read::Language;
use strum::VariantArray;

use crate::javascript::JavaScriptSyntaxProvider;
use crate::json::JsonSyntaxProvider;
use crate::markdown::MarkdownSyntaxProvider;
use crate::provider::SyntaxProvider;
use crate::python::PythonSyntaxProvider;
use crate::rust::RustSyntaxProvider;
use crate::toml::TomlSyntaxProvider;
use crate::typescript::TypeScriptDialect;
use crate::yaml::YamlSyntaxProvider;

/// One language and dialect this build ships, closed at compile time.
///
/// The wire `Language` stays open, since configuration may name languages no
/// build ships; this enum closes over what the build itself serves, so a
/// match on it cannot silently miss a shipped language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, VariantArray)]
pub enum ShippedLanguage {
    /// Rust.
    Rust,
    /// JavaScript, JSX included under the one pinned grammar.
    JavaScript,
    /// Plain TypeScript, where an angle bracket opens a type construct.
    TypeScript,
    /// The TSX dialect of TypeScript, where an angle bracket opens JSX.
    TypeScriptTsx,
    /// Markdown.
    Markdown,
    /// JSON.
    Json,
    /// YAML.
    Yaml,
    /// TOML.
    Toml,
    /// Python.
    Python,
}

impl ShippedLanguage {
    /// The wire identity this variant spells.
    #[must_use]
    pub fn language(self) -> Language {
        let (name, dialect) = match self {
            Self::Rust => ("rust", None),
            Self::JavaScript => ("javascript", None),
            Self::TypeScript => ("typescript", None),
            Self::TypeScriptTsx => ("typescript", Some("tsx")),
            Self::Markdown => ("markdown", None),
            Self::Json => ("json", None),
            Self::Yaml => ("yaml", None),
            Self::Toml => ("toml", None),
            Self::Python => ("python", None),
        };
        Language {
            name: name.to_owned(),
            dialect: dialect.map(str::to_owned),
        }
    }

    /// This build's definition for the language.
    ///
    /// # Panics
    ///
    /// Panics when the definition list carries no entry for the variant: a
    /// programmer error in [`definitions`], pinned by the registry's
    /// assembly and this module's own completeness test.
    #[must_use]
    pub fn definition(self) -> &'static dyn LanguageDefinition {
        definitions()
            .iter()
            .copied()
            .find(|definition| definition.shipped() == self)
            .unwrap_or_else(|| {
                unreachable!("every shipped language has one definition: variant={self:?}")
            })
    }
}

/// One shipped language's build contract: its identity, the file extensions
/// it claims, and the syntax tier parsing them.
pub trait LanguageDefinition: std::fmt::Debug + Send + Sync {
    /// The closed identity this definition serves.
    fn shipped(&self) -> ShippedLanguage;

    /// File extensions this language claims, without their leading dot.
    /// Extensions are unique across [`definitions`]: the workspace walk
    /// includes a file as source exactly when some definition claims its
    /// extension, and a workspace replaces the derived patterns per language
    /// with `[languages.<identity>] include`.
    fn extensions(&self) -> &'static [&'static str];

    /// The syntax provider parsing this language's sources, under its
    /// declared default bounds.
    fn syntax_provider(&self) -> Box<dyn SyntaxProvider>;
}

/// Every definition this build ships, in registration order: the one list a
/// new language joins.
#[must_use]
pub fn definitions() -> &'static [&'static dyn LanguageDefinition] {
    &[
        &RustDefinition,
        &JavaScriptDefinition,
        &TypeScriptDefinition,
        &TypeScriptTsxDefinition,
        &MarkdownDefinition,
        &JsonDefinition,
        &YamlDefinition,
        &TomlDefinition,
        &PythonDefinition,
    ]
}

/// Rust: `rs` files under the pinned tree-sitter-rust grammar.
#[derive(Debug)]
struct RustDefinition;

impl LanguageDefinition for RustDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Rust
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(RustSyntaxProvider::default())
    }
}

/// JavaScript: `js` and `jsx` files; the pinned grammar parses JSX, so `jsx`
/// needs no dialect of its own.
#[derive(Debug)]
struct JavaScriptDefinition;

impl LanguageDefinition for JavaScriptDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::JavaScript
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js", "jsx"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(JavaScriptSyntaxProvider::default())
    }
}

/// Plain TypeScript: `ts` files.
#[derive(Debug)]
struct TypeScriptDefinition;

impl LanguageDefinition for TypeScriptDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::TypeScript
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(TypeScriptDialect::TypeScript.provider())
    }
}

/// The TSX dialect: `tsx` files.
#[derive(Debug)]
struct TypeScriptTsxDefinition;

impl LanguageDefinition for TypeScriptTsxDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::TypeScriptTsx
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["tsx"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(TypeScriptDialect::Tsx.provider())
    }
}

/// Markdown: `md` files.
#[derive(Debug)]
struct MarkdownDefinition;

impl LanguageDefinition for MarkdownDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Markdown
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(MarkdownSyntaxProvider::default())
    }
}

/// JSON: `json` files.
#[derive(Debug)]
struct JsonDefinition;

impl LanguageDefinition for JsonDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Json
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(JsonSyntaxProvider::default())
    }
}

/// YAML: both spellings of the extension under one provider.
#[derive(Debug)]
struct YamlDefinition;

impl LanguageDefinition for YamlDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Yaml
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["yaml", "yml"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(YamlSyntaxProvider::default())
    }
}

/// TOML: `toml` files.
#[derive(Debug)]
struct TomlDefinition;

impl LanguageDefinition for TomlDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Toml
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["toml"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(TomlSyntaxProvider::default())
    }
}

/// Python: `py` sources and `pyi` stubs under one grammar.
#[derive(Debug)]
struct PythonDefinition;

impl LanguageDefinition for PythonDefinition {
    fn shipped(&self) -> ShippedLanguage {
        ShippedLanguage::Python
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(PythonSyntaxProvider::default())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn test_definitions_cover_every_shipped_language_exactly_once() {
        let listed: Vec<ShippedLanguage> = definitions()
            .iter()
            .map(|definition| definition.shipped())
            .collect();
        let distinct: BTreeSet<ShippedLanguage> = listed.iter().copied().collect();
        assert_eq!(
            listed.len(),
            distinct.len(),
            "no shipped language appears twice in the definition list"
        );
        assert_eq!(
            distinct,
            ShippedLanguage::VARIANTS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            "the definition list names every shipped language"
        );
    }

    #[test]
    fn test_definition_answers_each_variant_with_its_own_entry() {
        for variant in ShippedLanguage::VARIANTS {
            assert_eq!(variant.definition().shipped(), *variant);
        }
    }

    #[test]
    fn test_extensions_are_distinct_across_definitions() {
        let mut seen = BTreeSet::new();
        for definition in definitions() {
            for extension in definition.extensions() {
                assert!(
                    seen.insert(*extension),
                    "extension {extension} is claimed twice"
                );
            }
        }
    }

    #[test]
    fn test_each_definitions_provider_files_facts_under_its_own_identity() {
        for definition in definitions() {
            let provider = definition.syntax_provider();
            assert_eq!(
                provider.language(),
                &definition.shipped().language(),
                "provider and definition must spell one identity"
            );
        }
    }

    #[test]
    fn test_language_spells_the_identity_segment_with_its_dialect() {
        assert_eq!(ShippedLanguage::Rust.language().identity_segment(), "rust");
        assert_eq!(
            ShippedLanguage::TypeScriptTsx.language().identity_segment(),
            "typescript:tsx"
        );
        assert_eq!(
            ShippedLanguage::TypeScript.language().identity_segment(),
            "typescript"
        );
    }
}
