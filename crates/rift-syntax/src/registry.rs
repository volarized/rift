//! The shipped languages, selected by extension or language identity.
//!
//! The registry assembles from [`crate::language::definitions`]: one entry
//! per shipped definition, its provider constructed once at first use. A new
//! language joins the workspace through that list, never through this
//! module. Registry extensions select syntax facts after baseline file
//! discovery.

use std::sync::OnceLock;

use rift_protocol::read::Language;

use crate::language::{self, LanguageDefinition};
use crate::provider::SyntaxProvider;

/// One shipped definition beside the provider it constructed.
struct RegisteredLanguage {
    definition: &'static dyn LanguageDefinition,
    provider: Box<dyn SyntaxProvider>,
}

/// The shipped languages and the facts derived from them once, at first use.
struct SyntaxRegistry {
    entries: Vec<RegisteredLanguage>,
    extensions: Vec<&'static str>,
    file_bytes_max_default: usize,
}

impl SyntaxRegistry {
    /// Assembles one registry, proving the definition list's invariants.
    ///
    /// # Panics
    ///
    /// Panics when the definition list is empty, two definitions claim one
    /// extension, or a definition's provider files facts under another
    /// identity: each is a programmer error in the shipped definition list,
    /// not a reachable operating state.
    fn assemble(definitions: &[&'static dyn LanguageDefinition]) -> Self {
        assert!(
            !definitions.is_empty(),
            "the syntax registry must ship at least one language definition"
        );
        let mut extensions: Vec<&'static str> = Vec::new();
        let mut entries = Vec::with_capacity(definitions.len());
        for definition in definitions {
            for extension in definition.extensions() {
                assert!(
                    !extensions.contains(extension),
                    "language definitions must claim distinct extensions: \
                     extension={extension}, language={:?}",
                    definition.shipped(),
                );
                extensions.push(extension);
            }
            let provider = definition.syntax_provider();
            assert_eq!(
                provider.language(),
                &definition.shipped().language(),
                "a definition's provider must file facts under the definition's own identity"
            );
            entries.push(RegisteredLanguage {
                definition: *definition,
                provider,
            });
        }
        let file_bytes_max_default = entries
            .iter()
            .map(|entry| entry.provider.source_bytes_max())
            .max()
            .unwrap_or_else(|| {
                unreachable!("a non-empty definition set must have a maximum source byte bound")
            });
        Self {
            entries,
            extensions,
            file_bytes_max_default,
        }
    }
}

/// Returns the process-wide registry, assembling it once.
fn registry() -> &'static SyntaxRegistry {
    static REGISTRY: OnceLock<SyntaxRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| SyntaxRegistry::assemble(language::definitions()))
}

/// Every shipped provider, in registration order.
pub fn providers() -> impl Iterator<Item = &'static dyn SyntaxProvider> {
    registry()
        .entries
        .iter()
        .map(|entry| entry.provider.as_ref())
}

/// Every shipped definition beside its provider, in registration order.
pub fn shipped_languages()
-> impl Iterator<Item = (&'static dyn LanguageDefinition, &'static dyn SyntaxProvider)> {
    registry()
        .entries
        .iter()
        .map(|entry| (entry.definition, entry.provider.as_ref()))
}

/// The provider claiming `extension` (without its leading dot); `None` when
/// no shipped definition claims it.
#[must_use]
pub fn provider_for_extension(extension: &str) -> Option<&'static dyn SyntaxProvider> {
    registry()
        .entries
        .iter()
        .find(|entry| entry.definition.extensions().contains(&extension))
        .map(|entry| entry.provider.as_ref())
}

/// The provider filing facts under `language`; `None` when no shipped
/// grammar serves it.
#[must_use]
pub fn provider_for_language(language: &Language) -> Option<&'static dyn SyntaxProvider> {
    providers().find(|provider| provider.language() == language)
}

/// File extensions some shipped definition claims: the union of every
/// definition's declared extensions. The workspace walk includes a file as
/// source exactly when its extension is listed here.
#[must_use]
pub fn source_file_extensions() -> &'static [&'static str] {
    &registry().extensions
}

/// The largest per-source byte bound any shipped provider accepts by
/// default. The workspace's default per-file bound derives from it, so no
/// provider's default is unreachable under the scan.
#[must_use]
pub fn file_bytes_max_default() -> usize {
    registry().file_bytes_max_default
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_protocol::read::{Language, NodeFacet};

    use super::*;
    use crate::document::SyntaxDocument;
    use crate::failure::SyntaxError;
    use crate::language::ShippedLanguage;
    use crate::provider::SyntaxSource;
    use crate::rust::RustSyntaxProvider;

    fn rust() -> Language {
        Language {
            name: "rust".to_owned(),
            dialect: None,
        }
    }

    #[test]
    fn test_provider_for_extension_serves_rust_and_refuses_unclaimed() {
        let provider = provider_for_extension("rs").expect("the rust provider claims rs");
        assert_eq!(provider.language(), &rust());
        assert!(provider_for_extension("py").is_none());
    }

    #[test]
    fn test_provider_for_language_serves_rust_and_refuses_unserved() {
        let provider = provider_for_language(&rust()).expect("the rust provider serves rust");
        assert_eq!(provider.language(), &rust());
        let unserved = Language {
            name: "python".to_owned(),
            dialect: None,
        };
        assert!(provider_for_language(&unserved).is_none());
    }

    #[test]
    fn test_source_file_extensions_union_lists_every_declared_extension() {
        assert_eq!(
            source_file_extensions(),
            [
                "rs", "js", "jsx", "ts", "tsx", "md", "json", "yaml", "yml", "toml"
            ]
        );
    }

    /// Every definition's claimed extensions route back to the provider
    /// filing facts under that definition's own identity.
    #[test]
    fn test_each_claimed_extension_routes_to_its_definitions_provider() {
        for (definition, _provider) in shipped_languages() {
            let language = definition.shipped().language();
            for extension in definition.extensions() {
                let claimed = provider_for_extension(extension)
                    .unwrap_or_else(|| panic!("a shipped definition claims {extension}"));
                assert_eq!(
                    claimed.language(),
                    &language,
                    "extension {extension} must route to its own language"
                );
            }
        }
    }

    #[test]
    fn test_provider_for_language_separates_the_typescript_dialects() {
        let tsx = ShippedLanguage::TypeScriptTsx.language();
        let provider = provider_for_language(&tsx).expect("the tsx dialect is shipped");
        assert_eq!(provider.language(), &tsx);
        let plain = ShippedLanguage::TypeScript.language();
        let provider = provider_for_language(&plain).expect("plain typescript is shipped");
        assert_eq!(provider.language(), &plain);
    }

    #[test]
    fn test_file_bytes_max_default_is_the_largest_declared_provider_bound() {
        assert_eq!(
            file_bytes_max_default(),
            RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    #[test]
    fn test_registry_analyzes_through_the_trait_object() {
        let path = ProjectPath::new("src/lib.rs").expect("valid fixture path");
        let provider = provider_for_extension("rs").expect("the rust provider claims rs");
        let document = provider
            .analyze(SyntaxSource {
                path: &path,
                text: "pub fn beacon() {}",
            })
            .expect("fixture must parse");
        assert_eq!(document.language(), &rust());
        assert_eq!(document.symbols()[0].name, "beacon");
    }

    /// A provider proving nothing rust-specific leaks into the registry
    /// invariants, constructed by the stub definitions below.
    #[derive(Debug)]
    struct StubProvider {
        language: Language,
    }

    impl SyntaxProvider for StubProvider {
        fn language(&self) -> &Language {
            &self.language
        }

        fn source_bytes_max(&self) -> usize {
            1
        }

        fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
            Ok(SyntaxDocument::new(
                self.language.clone(),
                source.path.clone(),
                Vec::new(),
                Vec::new(),
                false,
            ))
        }

        fn node_facets(&self, _kind: &str) -> Vec<NodeFacet> {
            Vec::new()
        }
    }

    /// One definition shape for every invariant fixture: which identity it
    /// claims, which extensions, and which identity its provider files
    /// under, so each refusal test states only the fact it breaks.
    #[derive(Debug)]
    struct StubDefinition {
        shipped: ShippedLanguage,
        extensions: &'static [&'static str],
        provider_language: ShippedLanguage,
    }

    impl crate::language::LanguageDefinition for StubDefinition {
        fn shipped(&self) -> ShippedLanguage {
            self.shipped
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.extensions
        }

        fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
            Box::new(StubProvider {
                language: self.provider_language.language(),
            })
        }
    }

    /// A definition claiming the rust extension beside the real one.
    static RUST_EXTENSION_TWICE: StubDefinition = StubDefinition {
        shipped: ShippedLanguage::Json,
        extensions: &["rs"],
        provider_language: ShippedLanguage::Json,
    };

    /// A definition whose provider files facts under another identity.
    static MISMATCHED_IDENTITY: StubDefinition = StubDefinition {
        shipped: ShippedLanguage::Json,
        extensions: &["stub"],
        provider_language: ShippedLanguage::Yaml,
    };

    #[test]
    #[should_panic(expected = "language definitions must claim distinct extensions")]
    fn test_assemble_refuses_two_definitions_claiming_one_extension() {
        let rust = crate::language::definitions()[0];
        SyntaxRegistry::assemble(&[rust, &RUST_EXTENSION_TWICE]);
    }

    #[test]
    fn test_stub_provider_binding_layout_defaults_to_none() {
        let stub = StubProvider {
            language: ShippedLanguage::Json.language(),
        };
        assert!(
            stub.binding_layout(&["Cargo.toml", "src/lib.rs"]).is_none(),
            "a provider without module layout rules serves none by default"
        );
    }

    #[test]
    #[should_panic(expected = "the syntax registry must ship at least one language definition")]
    fn test_assemble_refuses_an_empty_definition_set() {
        SyntaxRegistry::assemble(&[]);
    }

    #[test]
    #[should_panic(expected = "a definition's provider must file facts under")]
    fn test_assemble_refuses_a_provider_filing_under_another_identity() {
        SyntaxRegistry::assemble(&[&MISMATCHED_IDENTITY]);
    }
}
