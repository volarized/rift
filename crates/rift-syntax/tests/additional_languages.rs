//! Syntax declarations, source fidelity, restoration, and bounds for shipped source grammars.

use rift_core::ProjectPath;
use rift_protocol::read::NodeFacet;
use rift_syntax::{
    ShippedLanguage, SyntaxDocument, SyntaxFacts, SyntaxFactsParts, SyntaxLimits, SyntaxNames,
    SyntaxSource,
};

fn parsed(
    shipped: ShippedLanguage,
    path: &str,
    text: &str,
    limits: SyntaxLimits,
) -> SyntaxDocument {
    let path = ProjectPath::new(path).expect("fixture path");
    shipped
        .definition()
        .syntax_provider()
        .analyze(SyntaxSource { path: &path, text }, limits)
        .expect("fixture parses within its bounds")
}

fn assert_node_facets(
    shipped: ShippedLanguage,
    text: &str,
    expected: &[(&str, &str)],
    facet: NodeFacet,
) {
    let document = parsed(shipped, "source", text, SyntaxLimits::default());
    assert!(!document.has_errors());
    let provider = shipped.definition().syntax_provider();
    for (kind, source) in expected {
        let node = document
            .nodes()
            .iter()
            .find(|node| node.kind == *kind)
            .unwrap_or_else(|| panic!("missing {kind}: {:?}", document.nodes()));
        let start = usize::try_from(node.range.start).expect("fixture start");
        let end = usize::try_from(node.range.end).expect("fixture end");
        assert_eq!(&text[start..end], *source);
        assert_eq!(provider.node_facets(node.kind), [facet]);
    }
}

#[test]
fn css_literal_nodes_preserve_original_values_and_facets() {
    assert_node_facets(
        ShippedLanguage::Css,
        ".lamp { color: #fff; width: 12px; content: \"灯\"; }",
        &[
            ("color_value", "#fff"),
            ("integer_value", "12px"),
            ("string_value", "\"灯\""),
        ],
        NodeFacet::Literal,
    );
}

#[test]
fn html_declaration_nodes_preserve_original_elements_and_attributes() {
    let text = "<section title=\"灯\">beacon</section>";
    assert_node_facets(
        ShippedLanguage::Html,
        text,
        &[("element", text), ("attribute", "title=\"灯\"")],
        NodeFacet::Declaration,
    );
}

#[test]
fn native_declarations_keep_names_ranges_and_language_identity() {
    let cases = [
        (
            ShippedLanguage::C,
            "src/client.c",
            "#include \"client.h\"\nstruct Client { int port; };\nint open_client(void) { return 1; }\n",
            "open_client",
        ),
        (
            ShippedLanguage::Cpp,
            "src/client.cpp",
            "namespace client { class Client { public: int open() { return 1; } }; }\n",
            "Client",
        ),
        (
            ShippedLanguage::Cython,
            "src/client.pyx",
            "include \"client.pxi\"\ncdef class Client:\n    cpdef int open(self):\n        return 1\n",
            "Client",
        ),
        (
            ShippedLanguage::Jsonc,
            "config.jsonc",
            "// client settings\n{\"client\": {\"port\": 8080 /* local */}}\n",
            "port",
        ),
        (
            ShippedLanguage::Css,
            "client.css",
            "@import \"base.css\";\n.client { --accent: blue; color: var(--accent); }\n@media screen { .client { display: flex; } }\n",
            "--accent",
        ),
        (
            ShippedLanguage::Html,
            "client.html",
            "<!doctype html><main id=\"client\"><a href=\"client.html\">Client</a></main>",
            "main",
        ),
    ];
    for (shipped, path, source, expected) in cases {
        let document = parsed(shipped, path, source, SyntaxLimits::default());
        assert_eq!(document.language(), &shipped.language());
        assert!(
            !document.has_errors(),
            "fixture for {shipped:?} has parser errors"
        );
        let symbol = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == expected)
            .unwrap_or_else(|| {
                panic!(
                    "{shipped:?} must declare {expected}: {:?}",
                    document.symbols()
                )
            });
        let start = usize::try_from(symbol.range.start).expect("fixture start");
        let end = usize::try_from(symbol.range.end).expect("fixture end");
        assert!(source[start..end].contains(expected));
        for node in document.nodes() {
            let start = usize::try_from(node.range.start).expect("fixture start");
            let end = usize::try_from(node.range.end).expect("fixture end");
            assert!(source.get(start..end).is_some());
        }
        let facts = document.facts();
        let restored = SyntaxFacts::from_parts(
            source,
            SyntaxLimits::default(),
            SyntaxFactsParts {
                origin: facts.origin(),
                language: facts.language().clone(),
                symbols: facts.symbols().to_vec(),
                has_errors: facts.has_errors(),
                left_out_declarations: facts.left_out_declaration_count(),
                markdown_facts: facts.markdown_facts().cloned(),
                source_digest: *document.source_digest().expect("captured source witness"),
            },
        )
        .expect("new grammar facts restore");
        assert_eq!(&restored, facts);
        let names = SyntaxNames::new(document.language()).expect("new grammar has retained names");
        for symbol in restored.symbols() {
            assert_eq!(names.symbol_kind(symbol.kind), Some(symbol.kind));
        }
    }
}

#[test]
fn new_source_providers_refuse_byte_node_and_depth_bounds() {
    let cases = [
        (ShippedLanguage::C, "int open(void) { return 1; }"),
        (
            ShippedLanguage::Cpp,
            "class Client { int open() { return 1; } };",
        ),
        (ShippedLanguage::Cython, "def open():\n    return 1\n"),
        (
            ShippedLanguage::Html,
            "<main><div><span>Client</span></div></main>",
        ),
        (ShippedLanguage::Css, ".client { color: blue; }"),
        (ShippedLanguage::Jsonc, "{\"client\": {\"port\": 8080}}"),
    ];
    let path = ProjectPath::new("source.fixture").expect("fixture path");
    for (shipped, text) in cases {
        let provider = shipped.definition().syntax_provider();
        for limits in [
            SyntaxLimits::new(1, 1000, 100).expect("byte bound"),
            SyntaxLimits::new(10000, 1, 100).expect("node bound"),
            SyntaxLimits::new(10000, 1000, 1).expect("depth bound"),
        ] {
            assert!(
                provider
                    .analyze(SyntaxSource { path: &path, text }, limits)
                    .is_err(),
                "{shipped:?} bound must refuse"
            );
        }
    }
}

#[test]
fn jsonc_and_json_keep_independent_registered_identities() {
    let text = "// configuration\n{\"client\": 1}";
    let json = parsed(
        ShippedLanguage::Json,
        "config.json",
        text,
        SyntaxLimits::default(),
    );
    let jsonc = parsed(
        ShippedLanguage::Jsonc,
        "config.jsonc",
        text,
        SyntaxLimits::default(),
    );
    assert_ne!(json.language(), jsonc.language());
    assert!(!json.has_errors());
    assert!(!jsonc.has_errors());
    assert_eq!(json.source_digest(), jsonc.source_digest());
}

#[test]
fn embedded_signatures_restore_only_registered_host_languages() {
    let source = "<main><script>function open() { return 1; }</script><style>.client { color: blue; }</style></main>";
    let document = parsed(
        ShippedLanguage::Html,
        "client.html",
        source,
        SyntaxLimits::default(),
    );
    let original = document.facts();
    for foreign in [
        ShippedLanguage::Rust.language(),
        ShippedLanguage::TypeScriptTsx.language(),
    ] {
        let mut symbols = original.symbols().to_vec();
        let script = symbols
            .iter_mut()
            .find(|symbol| symbol.name == "open")
            .expect("embedded declaration");
        std::sync::Arc::make_mut(&mut script.signatures)[0].language = foreign;
        let restored = SyntaxFacts::from_parts(
            source,
            SyntaxLimits::default(),
            SyntaxFactsParts {
                origin: original.origin(),
                language: original.language().clone(),
                symbols,
                has_errors: original.has_errors(),
                left_out_declarations: original.left_out_declaration_count(),
                markdown_facts: None,
                source_digest: *original.source_digest().expect("captured digest"),
            },
        );
        assert_eq!(
            restored.map_err(|error| error.slug().to_string()),
            Err("rift.syntax.facts_structure_invalid".to_owned())
        );
    }
}

#[test]
fn new_source_grammars_report_malformed_input_with_original_nodes() {
    for (shipped, source) in [
        (ShippedLanguage::C, "int open( {"),
        (ShippedLanguage::Cpp, "class Client {"),
        (ShippedLanguage::Cython, "def open(:\n"),
        (ShippedLanguage::Css, ".client { color:"),
        (ShippedLanguage::Html, "<div attr=\"unterminated"),
        (ShippedLanguage::Jsonc, "// settings\n{\"client\":"),
    ] {
        let document = parsed(shipped, "source.fixture", source, SyntaxLimits::default());
        assert!(
            document.has_errors(),
            "malformed fixture must report {shipped:?} errors"
        );
        assert!(!document.nodes().is_empty());
        assert_eq!(
            document.source_digest(),
            Some(&rift_core::FileDigest::of(source.as_bytes()))
        );
    }
}

#[test]
fn native_sources_preserve_includes_and_cython_declarations() {
    let cases = [
        (
            ShippedLanguage::C,
            "client.h",
            "#include \"base.h\"\n#define CLIENT_PORT 8080\nstruct Client { int port; };\nint open_client(void) { return CLIENT_PORT; }\n",
            &["\"base.h\"", "CLIENT_PORT", "Client", "open_client"][..],
        ),
        (
            ShippedLanguage::Cpp,
            "client.hpp",
            "#include <string>\nnamespace client { struct Client { int port; }; int open_client() { return 1; } }\n",
            &["<string>", "client", "Client", "open_client"][..],
        ),
        (
            ShippedLanguage::Cython,
            "client.pxd",
            "include \"base.pxi\"\ncdef int open_client(int port)\n",
            &["\"base.pxi\"", "open_client"][..],
        ),
        (
            ShippedLanguage::Cython,
            "client.pxi",
            "cdef int client_port = 8080\n",
            &["client_port"][..],
        ),
    ];
    for (shipped, path, source, names) in cases {
        let document = parsed(shipped, path, source, SyntaxLimits::default());
        assert!(!document.has_errors(), "fixture must parse for {shipped:?}");
        for expected in names {
            assert!(
                document
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.name == *expected),
                "{shipped:?} must preserve declaration {expected}"
            );
        }
    }
}
