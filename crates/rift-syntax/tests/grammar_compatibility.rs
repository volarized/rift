//! Proves every pinned grammar's compatibility with the pinned Tree-sitter
//! runtime.

use rift_core::ProjectPath;
use rift_syntax::{RustQuery, RustSyntaxProvider, SyntaxProvider, SyntaxSource};
use tree_sitter::{Language, MIN_COMPATIBLE_LANGUAGE_VERSION, Parser, Tree};

const REQUIRED_CAPTURE: &str = "rift.node";

/// Node kinds the ECMAScript rules read from every pinned grammar in the
/// family, restated here so a grammar bump that renames one fails this
/// suite before it fails extraction.
const ECMASCRIPT_CORE_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "method_definition",
    "variable_declarator",
    "lexical_declaration",
    "variable_declaration",
    "export_statement",
    "identifier",
];

/// Node kinds the ECMAScript rules read from the two TypeScript grammars
/// only.
const TYPESCRIPT_KINDS: &[&str] = &[
    "interface_declaration",
    "enum_declaration",
    "type_alias_declaration",
    "internal_module",
    "function_signature",
    "accessibility_modifier",
];

/// Grammar fields the ECMAScript rules read from every pinned grammar in
/// the family.
const ECMASCRIPT_FIELDS: &[&str] = &["name", "body", "value"];

/// Node kinds the markdown rules read from the pinned block grammar,
/// restated here so a grammar bump that renames one fails this suite before
/// it fails extraction.
const MARKDOWN_KINDS: &[&str] = &["section", "atx_heading", "setext_heading"];

/// Grammar fields the markdown rules read from the pinned block grammar.
const MARKDOWN_FIELDS: &[&str] = &["heading_content"];

/// Node kinds the JSON rules read from the pinned grammar, restated here so
/// a grammar bump that renames one fails this suite before it fails
/// extraction.
const JSON_KINDS: &[&str] = &["pair", "string"];

/// Grammar fields the JSON rules read from the pinned grammar.
const JSON_FIELDS: &[&str] = &["key", "value"];

/// Node kinds the YAML rules read from the pinned grammar, restated here so
/// a grammar bump that renames one fails this suite before it fails
/// extraction.
const YAML_KINDS: &[&str] = &[
    "stream",
    "document",
    "block_node",
    "flow_node",
    "block_mapping_pair",
    "flow_pair",
    "plain_scalar",
    "single_quote_scalar",
    "double_quote_scalar",
    "block_scalar",
];

/// Grammar fields the YAML rules read from the pinned grammar.
const YAML_FIELDS: &[&str] = &["key", "value"];

/// Node kinds the TOML rules read from the pinned grammar, restated here so
/// a grammar bump that renames one fails this suite before it fails
/// extraction. The pinned grammar declares no fields at all, so there is no
/// matching `TOML_FIELDS` constant.
const TOML_KINDS: &[&str] = &[
    "pair",
    "table",
    "table_array_element",
    "bare_key",
    "quoted_key",
    "dotted_key",
    "comment",
];

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn javascript_language() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn typescript_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

fn markdown_language() -> Language {
    tree_sitter_md::LANGUAGE.into()
}

fn json_language() -> Language {
    tree_sitter_json::LANGUAGE.into()
}

fn yaml_language() -> Language {
    tree_sitter_yaml::LANGUAGE.into()
}

fn toml_language() -> Language {
    tree_sitter_toml_ng::LANGUAGE.into()
}

fn assert_abi_in_runtime_window(name: &str, language: &Language) {
    assert!(
        (MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
            .contains(&language.abi_version()),
        "{name} grammar ABI must fit the pinned Tree-sitter runtime: abi_version={}",
        language.abi_version(),
    );
}

fn assert_kinds_resolve(name: &str, language: &Language, kinds: &[&str]) {
    for kind in kinds {
        assert_ne!(
            language.id_for_node_kind(kind, true),
            0,
            "pinned {name} grammar must define node kind used by symbol \
             extraction: kind={kind}",
        );
    }
}

fn assert_fields_resolve(name: &str, language: &Language, fields: &[&str]) {
    for field in fields {
        assert!(
            language.field_id_for_name(field).is_some(),
            "pinned {name} grammar must define field used by symbol \
             extraction: field={field}",
        );
    }
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

#[test]
fn test_rust_grammar_resolves_attachment_node_kind_ids() {
    let language = rust_language();
    for kind in ["attribute_item", "line_comment", "block_comment"] {
        assert_ne!(
            language.id_for_node_kind(kind, true),
            0,
            "pinned Rust grammar must define node kind used by attachment \
             classification: kind={kind}",
        );
    }
}

#[test]
fn test_rust_grammar_resolves_every_binding_kind_and_field() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let document = RustSyntaxProvider::default()
        .analyze(SyntaxSource {
            path: &path,
            text: "pub fn beacon() {}\n",
        })
        .expect("document");
    assert!(
        document.binding().is_some(),
        "resolving the binding grammar asserts every kind and field id; \
         a grammar bump that drops one fails here, not in extraction",
    );
}

#[test]
fn test_javascript_grammar_uses_supported_abi() {
    assert_abi_in_runtime_window("JavaScript", &javascript_language());
}

#[test]
fn test_typescript_grammars_use_supported_abi() {
    assert_abi_in_runtime_window("TypeScript", &typescript_language());
    assert_abi_in_runtime_window("TSX", &tsx_language());
}

#[test]
fn test_javascript_grammar_resolves_every_read_kind_and_field() {
    let language = javascript_language();
    assert_kinds_resolve("JavaScript", &language, ECMASCRIPT_CORE_KINDS);
    assert_fields_resolve("JavaScript", &language, ECMASCRIPT_FIELDS);
}

#[test]
fn test_typescript_grammars_resolve_every_read_kind_and_field() {
    for (name, language) in [
        ("TypeScript", typescript_language()),
        ("TSX", tsx_language()),
    ] {
        assert_kinds_resolve(name, &language, ECMASCRIPT_CORE_KINDS);
        assert_kinds_resolve(name, &language, TYPESCRIPT_KINDS);
        assert_fields_resolve(name, &language, ECMASCRIPT_FIELDS);
    }
}

#[test]
fn test_javascript_grammar_parses_valid_and_malformed_fixtures() {
    let language = javascript_language();
    let valid_tree = parse(&language, include_str!("fixtures/javascript/valid.js"));
    assert!(
        !valid_tree.root_node().has_error(),
        "valid JavaScript with JSX must parse"
    );

    let malformed_tree = parse(&language, include_str!("fixtures/javascript/malformed.js"));
    assert!(
        malformed_tree.root_node().has_error(),
        "malformed JavaScript must report an error"
    );
}

#[test]
fn test_typescript_grammar_parses_valid_and_malformed_fixtures() {
    let language = typescript_language();
    let valid_tree = parse(&language, include_str!("fixtures/typescript/valid.ts"));
    assert!(
        !valid_tree.root_node().has_error(),
        "valid TypeScript must parse"
    );

    let malformed_tree = parse(&language, include_str!("fixtures/typescript/malformed.ts"));
    assert!(
        malformed_tree.root_node().has_error(),
        "malformed TypeScript must report an error"
    );
}

#[test]
fn test_tsx_grammar_parses_valid_and_malformed_fixtures() {
    let language = tsx_language();
    let valid_tree = parse(&language, include_str!("fixtures/tsx/valid.tsx"));
    assert!(!valid_tree.root_node().has_error(), "valid TSX must parse");

    let malformed_tree = parse(&language, include_str!("fixtures/tsx/malformed.tsx"));
    assert!(
        malformed_tree.root_node().has_error(),
        "malformed TSX must report an error"
    );
}

#[test]
fn test_markdown_block_grammar_uses_supported_abi() {
    assert_abi_in_runtime_window("markdown", &markdown_language());
}

#[test]
fn test_markdown_block_grammar_resolves_every_read_kind_and_field() {
    let language = markdown_language();
    assert_kinds_resolve("markdown", &language, MARKDOWN_KINDS);
    assert_fields_resolve("markdown", &language, MARKDOWN_FIELDS);
}

/// The block grammar accepts nearly any text as prose; the malformed
/// fixture is the case it does refuse - an ATX heading as the file's last
/// bytes with no trailing line ending.
#[test]
fn test_markdown_block_grammar_parses_valid_and_malformed_fixtures() {
    let language = markdown_language();
    let valid_tree = parse(&language, include_str!("fixtures/markdown/valid.md"));
    assert!(
        !valid_tree.root_node().has_error(),
        "valid markdown must parse"
    );

    let malformed_tree = parse(&language, include_str!("fixtures/markdown/malformed.md"));
    assert!(
        malformed_tree.root_node().has_error(),
        "an unterminated heading must report an error"
    );
}

#[test]
fn test_json_grammar_uses_supported_abi() {
    assert_abi_in_runtime_window("JSON", &json_language());
}

#[test]
fn test_json_grammar_resolves_every_read_kind_and_field() {
    let language = json_language();
    assert_kinds_resolve("JSON", &language, JSON_KINDS);
    assert_fields_resolve("JSON", &language, JSON_FIELDS);
}

/// The pinned grammar marks a pair missing its value; the malformed fixture
/// is exactly that pair.
#[test]
fn test_json_grammar_parses_valid_and_malformed_fixtures() {
    let language = json_language();
    let valid_tree = parse(&language, include_str!("fixtures/json/valid.json"));
    assert!(!valid_tree.root_node().has_error(), "valid JSON must parse");

    let malformed_tree = parse(&language, include_str!("fixtures/json/malformed.json"));
    assert!(
        malformed_tree.root_node().has_error(),
        "a pair missing its value must report an error"
    );
}

#[test]
fn test_yaml_grammar_uses_supported_abi() {
    assert_abi_in_runtime_window("YAML", &yaml_language());
}

#[test]
fn test_yaml_grammar_resolves_every_read_kind_and_field() {
    let language = yaml_language();
    assert_kinds_resolve("YAML", &language, YAML_KINDS);
    assert_fields_resolve("YAML", &language, YAML_FIELDS);
}

#[test]
fn test_yaml_grammar_parses_valid_and_malformed_fixtures() {
    let language = yaml_language();
    let valid_tree = parse(&language, include_str!("fixtures/yaml/valid.yaml"));
    assert!(!valid_tree.root_node().has_error(), "valid YAML must parse");

    let malformed_tree = parse(&language, include_str!("fixtures/yaml/malformed.yaml"));
    assert!(
        malformed_tree.root_node().has_error(),
        "an unclosed flow sequence must report an error"
    );
}

#[test]
fn test_toml_grammar_uses_supported_abi() {
    assert_abi_in_runtime_window("TOML", &toml_language());
}

/// The pinned TOML grammar declares no fields at all - only kind ids are
/// asserted here, and the field count is pinned to `0` so a grammar bump
/// that starts declaring fields is a fact this suite would want reviewed.
#[test]
fn test_toml_grammar_resolves_every_read_kind_and_declares_no_fields() {
    let language = toml_language();
    assert_kinds_resolve("TOML", &language, TOML_KINDS);
    assert_eq!(
        language.field_count(),
        0,
        "pinned TOML grammar must declare no fields"
    );
}

/// A pair with no value at all parses as an `ERROR` node wrapping the bare
/// key and `=`, not a `pair` node; the malformed fixture is exactly that.
#[test]
fn test_toml_grammar_parses_valid_and_malformed_fixtures() {
    let language = toml_language();
    let valid_tree = parse(&language, include_str!("fixtures/toml/valid.toml"));
    assert!(!valid_tree.root_node().has_error(), "valid TOML must parse");

    let malformed_tree = parse(&language, include_str!("fixtures/toml/malformed.toml"));
    assert!(
        malformed_tree.root_node().has_error(),
        "a pair missing its value must report an error"
    );
}

/// The dialect split at the grammar level: the TSX fixture's bytes break
/// through the plain TypeScript grammar, where an angle bracket opens a
/// type construct.
#[test]
fn test_tsx_fixture_breaks_through_the_plain_typescript_grammar() {
    let tree = parse(
        &typescript_language(),
        include_str!("fixtures/tsx/valid.tsx"),
    );
    assert!(
        tree.root_node().has_error(),
        "TSX bytes must report an error through plain TypeScript"
    );
}
