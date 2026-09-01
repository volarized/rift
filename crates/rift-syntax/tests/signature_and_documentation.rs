//! Common language test: `SyntaxSymbol.signatures` and `.documentation` are provider-produced
//! facts, proven once per registered provider rather than once per language module.
//!
//! The loop walks [`registry::providers`], so a provider registered later joins this proof
//! without an edit here - only [`fixtures_for`] gains a case naming its language.

use rift_core::ProjectPath;
use rift_protocol::read::Language;
use rift_syntax::{SyntaxDocument, SyntaxSource, registry};

/// Fixtures proving one provider's `signatures` and `documentation` facts.
///
/// `neither` is a declaration whose grammar attaches neither fact - every provider supplies
/// one. `header` and `documentation` are `None` for a language whose grammar never attaches
/// the corresponding fact at all (JSON, TOML, YAML, markdown carry no callable form and no
/// attachable comment today), which [`SyntaxSymbol::signatures`] and `.documentation` answer
/// with two empty lists, a correct answer rather than a gap.
struct ProviderFixtures {
    neither: &'static str,
    header: Option<&'static str>,
    documentation: Option<&'static str>,
}

/// The fixtures for `language`, by name and dialect. Panics when a registered provider names
/// a language this table has no entry for - the deliberate failure mode that tells the next
/// implementer to add one.
fn fixtures_for(language: &Language) -> ProviderFixtures {
    match (language.name.as_str(), language.dialect.as_deref()) {
        ("rust", _) => ProviderFixtures {
            neither: "pub struct Beacon;\n",
            header: Some("pub fn beacon() -> i32 {\n    0\n}\n"),
            documentation: Some("/// Beacon docs.\npub fn beacon() -> i32 {\n    0\n}\n"),
        },
        ("javascript", _) => ProviderFixtures {
            neither: "const beacon = 1;\n",
            header: Some("function beacon() {\n  return 1;\n}\n"),
            documentation: None,
        },
        ("typescript", _) => ProviderFixtures {
            neither: "const beacon: number = 1;\n",
            header: Some("function beacon(): number {\n  return 1;\n}\n"),
            documentation: None,
        },
        ("markdown", _) => ProviderFixtures {
            neither: "# Beacon\n\nSome text.\n",
            header: None,
            documentation: None,
        },
        ("json", _) => ProviderFixtures {
            neither: "{\"beacon\": 1}\n",
            header: None,
            documentation: None,
        },
        ("yaml", _) => ProviderFixtures {
            neither: "beacon: 1\n",
            header: None,
            documentation: None,
        },
        ("toml", _) => ProviderFixtures {
            neither: "beacon = 1\n",
            header: None,
            documentation: None,
        },
        ("python", _) => ProviderFixtures {
            neither: "BEACON = 1\n",
            header: Some("def beacon():\n    return 1\n"),
            documentation: Some("def beacon():\n    \"Beacon docs.\"\n    return 1\n"),
        },
        (name, dialect) => panic!(
            "a registered provider has no signature/documentation fixture: \
             language={name}, dialect={dialect:?}"
        ),
    }
}

fn path() -> ProjectPath {
    ProjectPath::new("fixture").expect("valid fixture path")
}

fn analyze(provider: &dyn rift_syntax::SyntaxProvider, text: &str) -> SyntaxDocument {
    provider
        .analyze(SyntaxSource {
            path: &path(),
            text,
        })
        .expect("fixture must parse")
}

/// The one symbol a fixture's declaration produced.
fn declared_symbol(document: &SyntaxDocument) -> &rift_syntax::SyntaxSymbol {
    document
        .symbols()
        .first()
        .expect("fixture must declare exactly one symbol")
}

#[test]
fn every_provider_fixture_with_neither_fact_attached_returns_two_empty_lists() {
    for provider in registry::providers() {
        let fixtures = fixtures_for(provider.language());
        let document = analyze(provider, fixtures.neither);
        let symbol = declared_symbol(&document);
        assert!(
            symbol.signatures.is_empty(),
            "language={:?} fixture={:?} symbol={:?}: signatures must stay empty with no \
             callable form attached",
            provider.language(),
            fixtures.neither,
            symbol,
        );
        assert!(
            symbol.documentation.is_empty(),
            "language={:?} fixture={:?} symbol={:?}: documentation must stay empty with \
             nothing attached",
            provider.language(),
            fixtures.neither,
            symbol,
        );
    }
}

#[test]
fn every_provider_fixture_with_an_attached_declaration_header_returns_a_signature() {
    for provider in registry::providers() {
        let Some(header) = fixtures_for(provider.language()).header else {
            continue;
        };
        let document = analyze(provider, header);
        let symbol = declared_symbol(&document);
        assert!(
            !symbol.signatures.is_empty(),
            "language={:?} fixture={header:?}: a callable declaration with a body must \
             return a signature: symbol={symbol:?}",
            provider.language(),
        );
        assert!(
            !symbol.signatures[0].display.is_empty(),
            "language={:?}: the rendered signature must carry its header text: symbol={symbol:?}",
            provider.language(),
        );
    }
}

#[test]
fn every_provider_fixture_with_attached_documentation_returns_it() {
    for provider in registry::providers() {
        let Some(documented) = fixtures_for(provider.language()).documentation else {
            continue;
        };
        let document = analyze(provider, documented);
        let symbol = declared_symbol(&document);
        assert!(
            !symbol.documentation.is_empty(),
            "language={:?} fixture={documented:?}: an attached doc comment must return \
             documentation: symbol={symbol:?}",
            provider.language(),
        );
        assert!(
            !symbol.documentation[0].text.is_empty(),
            "language={:?}: the attached documentation must carry its written text: \
             symbol={symbol:?}",
            provider.language(),
        );
    }
}

/// At least one registered provider exercises each branch, so the loops above are not
/// vacuously true.
#[test]
fn the_registry_carries_at_least_one_provider_for_every_fixture_kind() {
    let providers: Vec<_> = registry::providers().collect();
    assert!(
        providers
            .iter()
            .any(|provider| fixtures_for(provider.language()).header.is_some()),
        "at least one registered provider must exercise the header fixture"
    );
    assert!(
        providers
            .iter()
            .any(|provider| fixtures_for(provider.language()).documentation.is_some()),
        "at least one registered provider must exercise the documentation fixture"
    );
}
