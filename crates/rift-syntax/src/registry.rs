//! The shipped syntax providers, selected by extension or language.
//!
//! A new language joins the workspace by adding its provider module, its
//! grammar dependency, and one entry in this module's shipped provider
//! list. Registry extensions select syntax facts after baseline file discovery.

use std::sync::OnceLock;

use rift_protocol::read::Language;

use crate::javascript::JavaScriptSyntaxProvider;
use crate::json::JsonSyntaxProvider;
use crate::markdown::MarkdownSyntaxProvider;
use crate::provider::SyntaxProvider;
use crate::rust::RustSyntaxProvider;
use crate::toml::TomlSyntaxProvider;
use crate::typescript::TypeScriptDialect;
use crate::yaml::YamlSyntaxProvider;

/// Every provider this build ships, in registration order.
fn build_registry() -> Vec<Box<dyn SyntaxProvider>> {
    vec![
        Box::new(RustSyntaxProvider::default()),
        Box::new(JavaScriptSyntaxProvider::default()),
        Box::new(TypeScriptDialect::TypeScript.provider()),
        Box::new(TypeScriptDialect::Tsx.provider()),
        Box::new(MarkdownSyntaxProvider::default()),
        Box::new(JsonSyntaxProvider::default()),
        Box::new(YamlSyntaxProvider::default()),
        Box::new(TomlSyntaxProvider::default()),
    ]
}

/// The shipped providers and the facts derived from them once, at first use.
struct SyntaxRegistry {
    providers: Vec<Box<dyn SyntaxProvider>>,
    extensions: Vec<&'static str>,
    file_bytes_max_default: usize,
}

impl SyntaxRegistry {
    /// Assembles one registry, proving the provider set's invariants.
    ///
    /// # Panics
    ///
    /// Panics when the provider set is empty or two providers claim one
    /// extension: both are programmer errors in the shipped provider list,
    /// not reachable operating states.
    fn assemble(providers: Vec<Box<dyn SyntaxProvider>>) -> Self {
        assert!(
            !providers.is_empty(),
            "the syntax registry must ship at least one provider"
        );
        let mut extensions: Vec<&'static str> = Vec::new();
        for provider in &providers {
            for extension in provider.extensions() {
                assert!(
                    !extensions.contains(extension),
                    "syntax providers must claim distinct extensions: \
                     extension={extension}, language={}",
                    provider.language().name,
                );
                extensions.push(extension);
            }
        }
        let file_bytes_max_default = providers
            .iter()
            .map(|provider| provider.source_bytes_max())
            .max()
            .unwrap_or_else(|| {
                unreachable!("a non-empty provider set must have a maximum source byte bound")
            });
        Self {
            providers,
            extensions,
            file_bytes_max_default,
        }
    }
}

/// Returns the process-wide registry, assembling it once.
fn registry() -> &'static SyntaxRegistry {
    static REGISTRY: OnceLock<SyntaxRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| SyntaxRegistry::assemble(build_registry()))
}

/// Every shipped provider, in registration order.
pub fn providers() -> impl Iterator<Item = &'static dyn SyntaxProvider> {
    registry().providers.iter().map(Box::as_ref)
}

/// The provider claiming `extension` (without its leading dot); `None` when
/// no shipped grammar parses it.
#[must_use]
pub fn provider_for_extension(extension: &str) -> Option<&'static dyn SyntaxProvider> {
    providers().find(|provider| provider.extensions().contains(&extension))
}

/// The provider filing facts under `language`; `None` when no shipped
/// grammar serves it.
#[must_use]
pub fn provider_for_language(language: &Language) -> Option<&'static dyn SyntaxProvider> {
    providers().find(|provider| provider.language() == language)
}

/// File extensions some shipped provider parses: the union of every
/// provider's declared extensions. The workspace walk includes a file as
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
    use crate::provider::SyntaxSource;

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
        assert_eq!(provider.extensions(), ["rs"]);
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

    #[test]
    fn test_provider_for_extension_serves_markdown() {
        let provider = provider_for_extension("md").expect("the markdown provider claims md");
        assert_eq!(provider.language().name, "markdown");
        assert_eq!(provider.language().dialect, None);
        let language = Language {
            name: "markdown".to_owned(),
            dialect: None,
        };
        let by_language =
            provider_for_language(&language).expect("the markdown provider serves markdown");
        assert_eq!(by_language.extensions(), ["md"]);
    }

    #[test]
    fn test_provider_for_extension_serves_json() {
        let provider = provider_for_extension("json").expect("the JSON provider claims json");
        assert_eq!(provider.language().name, "json");
        assert_eq!(provider.language().dialect, None);
        let language = Language {
            name: "json".to_owned(),
            dialect: None,
        };
        let by_language = provider_for_language(&language).expect("the JSON provider serves json");
        assert_eq!(by_language.extensions(), ["json"]);
    }

    /// One YAML provider claims both spellings of the extension.
    #[test]
    fn test_provider_for_extension_serves_yaml_under_both_extensions() {
        for extension in ["yaml", "yml"] {
            let provider = provider_for_extension(extension)
                .unwrap_or_else(|| panic!("the YAML provider claims {extension}"));
            assert_eq!(provider.language().name, "yaml");
            assert_eq!(provider.language().dialect, None);
        }
        let language = Language {
            name: "yaml".to_owned(),
            dialect: None,
        };
        let by_language = provider_for_language(&language).expect("the YAML provider serves yaml");
        assert_eq!(by_language.extensions(), ["yaml", "yml"]);
    }

    #[test]
    fn test_provider_for_extension_serves_toml() {
        let provider = provider_for_extension("toml").expect("the TOML provider claims toml");
        assert_eq!(provider.language().name, "toml");
        assert_eq!(provider.language().dialect, None);
        let language = Language {
            name: "toml".to_owned(),
            dialect: None,
        };
        let by_language = provider_for_language(&language).expect("the TOML provider serves toml");
        assert_eq!(by_language.extensions(), ["toml"]);
    }

    #[test]
    fn test_provider_for_extension_routes_each_ecmascript_extension_to_its_dialect() {
        let routes = [
            ("js", "javascript", None),
            ("jsx", "javascript", None),
            ("ts", "typescript", None),
            ("tsx", "typescript", Some("tsx")),
        ];
        for (extension, name, dialect) in routes {
            let provider = provider_for_extension(extension)
                .unwrap_or_else(|| panic!("a shipped provider claims {extension}"));
            assert_eq!(provider.language().name, name);
            assert_eq!(provider.language().dialect.as_deref(), dialect);
        }
    }

    #[test]
    fn test_provider_for_language_separates_the_typescript_dialects() {
        let tsx = Language {
            name: "typescript".to_owned(),
            dialect: Some("tsx".to_owned()),
        };
        let provider = provider_for_language(&tsx).expect("the tsx dialect is shipped");
        assert_eq!(provider.extensions(), ["tsx"]);
        let plain = Language {
            name: "typescript".to_owned(),
            dialect: None,
        };
        let provider = provider_for_language(&plain).expect("plain typescript is shipped");
        assert_eq!(provider.extensions(), ["ts"]);
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

    #[test]
    #[should_panic(expected = "syntax providers must claim distinct extensions")]
    fn test_assemble_refuses_two_providers_claiming_one_extension() {
        SyntaxRegistry::assemble(vec![
            Box::new(RustSyntaxProvider::default()),
            Box::new(RustSyntaxProvider::default()),
        ]);
    }

    #[test]
    #[should_panic(expected = "the syntax registry must ship at least one provider")]
    fn test_assemble_refuses_an_empty_provider_set() {
        SyntaxRegistry::assemble(Vec::new());
    }

    /// A provider proving nothing rust-specific leaks into the registry
    /// invariants: distinct extensions assemble beside the rust provider.
    #[derive(Debug)]
    struct StubProvider {
        language: Language,
    }

    impl SyntaxProvider for StubProvider {
        fn language(&self) -> &Language {
            &self.language
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["stub"]
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

    #[test]
    fn test_stub_provider_binding_layout_defaults_to_none() {
        let stub = StubProvider {
            language: Language {
                name: "stub".to_owned(),
                dialect: None,
            },
        };
        assert!(
            stub.binding_layout(&["Cargo.toml", "src/lib.rs"]).is_none(),
            "a provider without module layout rules serves none by default"
        );
    }

    #[test]
    fn test_assemble_accepts_distinct_extensions_and_takes_the_larger_byte_bound() {
        let stub = StubProvider {
            language: Language {
                name: "stub".to_owned(),
                dialect: None,
            },
        };
        let assembled = SyntaxRegistry::assemble(vec![
            Box::new(RustSyntaxProvider::default()),
            Box::new(stub),
        ]);
        assert_eq!(assembled.extensions, ["rs", "stub"]);
        assert_eq!(
            assembled.file_bytes_max_default,
            RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
        );
    }
}
