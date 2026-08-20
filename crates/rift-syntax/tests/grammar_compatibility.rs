//! Proves Rust grammar compatibility with pinned Tree-sitter runtime.

use rift_syntax::RustQuery;
use tree_sitter::{Language, MIN_COMPATIBLE_LANGUAGE_VERSION, Parser, Tree};

const REQUIRED_CAPTURE: &str = "rift.node";

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn parse(language: &Language, source: &str) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .unwrap_or_else(|error| panic!("grammar must use supported Tree-sitter ABI: {error}"));
    parser
        .parse(source, None)
        .unwrap_or_else(|| panic!("fixed fixture parse must not be cancelled"))
}

fn query_has_capture(language: &Language, source: &str, query_source: &str) -> bool {
    let _tree = parse(language, source);
    let query = RustQuery::new(query_source)
        .unwrap_or_else(|error| panic!("required capture query must compile: {error}"));
    assert_eq!(
        query.capture_names(),
        &[REQUIRED_CAPTURE],
        "query must expose exact required capture vocabulary",
    );
    assert!(query.has_capture(REQUIRED_CAPTURE));
    !query
        .captures(source, 1)
        .expect("bounded compatibility capture must execute")
        .is_empty()
}

#[test]
fn test_rust_grammar_parses_valid_and_malformed_fixtures() {
    let language = rust_language();
    let valid_tree = parse(&language, include_str!("fixtures/rust/valid.rs"));
    assert!(!valid_tree.root_node().has_error(), "valid Rust must parse");

    let malformed_tree = parse(&language, include_str!("fixtures/rust/malformed.rs"));
    assert!(
        malformed_tree.root_node().has_error(),
        "malformed Rust must report an error"
    );
}

#[test]
fn test_rust_grammar_compiles_required_capture_query() {
    let language = rust_language();
    assert!(
        query_has_capture(
            &language,
            include_str!("fixtures/rust/valid.rs"),
            "(source_file) @rift.node",
        ),
        "required query must capture Rust root",
    );
}

#[test]
fn test_rust_grammar_uses_supported_abi() {
    let rust = rust_language();
    assert!(
        (MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
            .contains(&rust.abi_version()),
        "Rust grammar ABI must fit Tree-sitter 0.25: abi_version={}",
        rust.abi_version(),
    );
}
