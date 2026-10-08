//! Captured provider facts retain their values through checked construction.

use rift_core::ProjectPath;
use rift_error::errors;
use rift_syntax::{
    ByteRange, MarkdownFacts, MarkdownFactsParts, ShippedLanguage, SyntaxDocument, SyntaxFacts,
    SyntaxFactsParts, SyntaxLimits, SyntaxNames, SyntaxSource,
};

fn parsed(shipped: ShippedLanguage, text: &str) -> SyntaxDocument {
    let language = shipped.language();
    let provider =
        rift_syntax::registry::provider_for_language(&language).expect("shipped provider");
    provider
        .analyze(
            SyntaxSource {
                path: &ProjectPath::new("source").expect("fixture path"),
                text,
            },
            SyntaxLimits::default(),
        )
        .expect("fixture parses")
}

fn parts(facts: &SyntaxFacts) -> SyntaxFactsParts {
    SyntaxFactsParts {
        language: facts.language().clone(),
        symbols: facts.symbols().to_vec(),
        has_errors: facts.has_errors(),
        left_out_declarations: facts.left_out_declaration_count(),
        markdown_facts: facts.markdown_facts().cloned(),
        source_digest: *facts.source_digest().expect("source witness"),
    }
}

fn markdown_parts(facts: &MarkdownFacts) -> MarkdownFactsParts {
    MarkdownFactsParts {
        blocks: facts.blocks().to_vec(),
        headings: facts.headings().to_vec(),
        links: facts.links().to_vec(),
        reference_candidates: facts.reference_candidates().to_vec(),
        error_ranges: facts.error_ranges().to_vec(),
        omitted_ranges: facts.omitted_ranges().to_vec(),
    }
}

#[test]
fn complete_facts_restore_for_every_shipped_language() {
    let cases = [
        (
            ShippedLanguage::Rust,
            "/// Opens a file.\npub mod client { pub fn open() {} }\n",
        ),
        (
            ShippedLanguage::JavaScript,
            "/** Opens a file. */\nexport class Client { open() {} }\n",
        ),
        (
            ShippedLanguage::TypeScript,
            "export interface Client { open(): void; }\n",
        ),
        (
            ShippedLanguage::TypeScriptTsx,
            "export class Client { open(): null { return null; } }\n",
        ),
        (
            ShippedLanguage::Python,
            "class Client:\n    def open(self):\n        \"\"\"Opens a file.\"\"\"\n        return 1\n",
        ),
        (
            ShippedLanguage::Markdown,
            "# Beacon\n\nSee [`Client`](guide.md#client).\n\n## Client\n\n```rust\npub fn open() {}\n```\n",
        ),
        (ShippedLanguage::Json, "{\"client\":{\"port\":8080}}"),
        (ShippedLanguage::Toml, "[client]\nport = 8080\n"),
        (ShippedLanguage::Yaml, "client:\n  port: 8080\n"),
    ];
    for (shipped, text) in cases {
        let document = parsed(shipped, text);
        let restored =
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
                .expect("complete restored facts");
        assert_eq!(&restored, document.facts(), "{shipped:?}");
        let names = SyntaxNames::new(document.language()).expect("shipped names");
        for symbol in restored.symbols() {
            let resolved_kind = {
                let decoded_kind = symbol.kind.to_owned();
                names.symbol_kind(&decoded_kind)
            };
            assert_eq!(resolved_kind, Some(symbol.kind));
            if let Some(kind) = symbol.node_kind {
                let resolved_node = {
                    let decoded_node = kind.to_owned();
                    names.node_kind(&decoded_node)
                };
                assert_eq!(resolved_node, Some(kind));
            }
        }
    }
}

#[test]
fn duplicate_suffixes_parser_errors_and_omissions_are_preserved() {
    let cases = ["pub fn open() {}\npub fn open() {}\n", "pub fn open( {\n"];
    for text in cases {
        let document = parsed(ShippedLanguage::Rust, text);
        let restored =
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
                .expect("normalized facts");
        assert_eq!(&restored, document.facts());
    }
    let text = format!(
        "pub fn {}() {{}}\n",
        "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1)
    );
    let document = parsed(ShippedLanguage::Rust, &text);
    assert!(document.left_out_declaration_count() > 0);
    let restored = SyntaxFacts::from_parts(&text, SyntaxLimits::default(), parts(document.facts()))
        .expect("omissions");
    assert_eq!(&restored, document.facts());
}

#[test]
fn invalid_ranges_names_order_and_source_witness_are_refused() {
    let text = "pub fn café() {}\npub fn close() {}\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let mut invalid = original.clone();
    invalid.source_digest = rift_core::FileDigest::of(b"different source");
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_source_mismatch::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols[0].range.end = u64::MAX;
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_range_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols[0].item_range = ByteRange { start: 1, end: 0 };
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_range_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols[0].name_range = Some(ByteRange {
        start: u64::try_from(text.find('é').expect("Unicode fixture") + 1).expect("offset"),
        end: 14,
    });
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_range_invalid::SLUG)
    );
    for name in [
        String::new(),
        "bad\nname".to_owned(),
        "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1),
    ] {
        let mut invalid = original.clone();
        invalid.symbols[0].name = name;
        assert_eq!(
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_name_invalid::SLUG)
        );
    }
    let mut invalid = original.clone();
    invalid.symbols[1]
        .qualified_name
        .clone_from(&original.symbols[0].qualified_name);
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_name_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols.reverse();
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_order_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols[0].kind = "unknown_kind";
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_kind_invalid::SLUG)
    );
    let mut invalid = original;
    invalid.symbols[0].node_kind = Some("unknown_node_kind");
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_kind_invalid::SLUG)
    );
}

#[test]
fn symbol_ranges_must_stay_inside_the_declaration() {
    let text = "pub fn open() {}\npub fn close() {}\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let outside = original.symbols[1].range;
    for field in 0..4 {
        let mut invalid = original.clone();
        match field {
            0 => invalid.symbols[0].item_range = outside,
            1 => invalid.symbols[0].name_range = Some(outside),
            2 => invalid.symbols[0].body_range = Some(outside),
            3 => invalid.symbols[0].documentation_ranges = vec![outside],
            _ => unreachable!("four declaration ranges"),
        }
        assert_eq!(
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_range_invalid::SLUG),
            "field {field}"
        );
    }
}

#[test]
fn authored_containers_need_portable_names_but_may_be_unresolved() {
    let text = "pub mod client { pub fn open() {} }\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let mut invalid = original.clone();
    invalid.symbols[1].container = Some("bad\nname".to_owned());
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_name_invalid::SLUG)
    );
    let mut authored = original;
    authored.symbols[1].container = Some("client::authored".to_owned());
    let restored = SyntaxFacts::from_parts(text, SyntaxLimits::default(), authored.clone())
        .expect("unresolved authored container");
    assert_eq!(restored.symbols(), authored.symbols);
}

#[test]
fn symbol_parent_cycles_and_exceeded_bounds_are_refused() {
    let text = "pub mod client { pub fn open() {} }\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let mut invalid = original.clone();
    invalid.symbols[0].container = Some(invalid.symbols[0].qualified_name.clone());
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.symbols[0].container = Some(invalid.symbols[1].qualified_name.clone());
    invalid.symbols[1].container = Some(invalid.symbols[0].qualified_name.clone());
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let limits = SyntaxLimits::new(text.len() - 1, 100, 100).expect("bounds");
    assert_eq!(
        SyntaxFacts::from_parts(text, limits, original.clone()).map_err(|error| error.slug()),
        Err(errors::syntax::facts_source_too_large::SLUG)
    );
    let limits = SyntaxLimits::new(text.len(), 1, 100).expect("bounds");
    assert_eq!(
        SyntaxFacts::from_parts(text, limits, original.clone()).map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG)
    );
    let limits = SyntaxLimits::new(text.len(), 100, 1).expect("bounds");
    assert_eq!(
        SyntaxFacts::from_parts(text, limits, original.clone()).map_err(|error| error.slug()),
        Err(errors::syntax::facts_depth_exceeded::SLUG)
    );
    let mut invalid = original;
    invalid.left_out_declarations = usize::MAX;
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG)
    );
}

#[test]
fn markdown_parent_cycles_invalid_indices_and_ranges_are_refused() {
    let text =
        "# Beacon\n\nSee [Client](guide.md#client) and `Client`.\n\n## Client\n\nOpen a file.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let facts = document.markdown_facts().expect("Markdown facts");
    let original = markdown_parts(facts);
    let mut invalid = original.clone();
    invalid.headings[0].parent = Some(0);
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.headings[0].parent = Some(1);
    invalid.headings[1].parent = Some(0);
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.headings[0].symbol_index = usize::MAX;
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.blocks[0].heading = Some(usize::MAX);
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.links[0].block_range = ByteRange { start: 0, end: 0 };
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.reference_candidates[0].range.end = u64::MAX;
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_range_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.blocks.reverse();
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_order_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.blocks[0].line = 0;
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_structure_invalid::SLUG)
    );
    let restored =
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), original)
            .expect("complete Markdown facts");
    assert_eq!(&restored, facts);
}

#[test]
fn unknown_language_dialect_and_provider_names_are_refused() {
    let mut language = ShippedLanguage::Rust.language();
    language.dialect = Some("unknown".to_owned());
    assert!(SyntaxNames::new(&language).is_none());
    let javascript =
        SyntaxNames::new(&ShippedLanguage::JavaScript.language()).expect("JavaScript names");
    assert!(javascript.symbol_kind("interface").is_none());
    assert!(javascript.node_kind("unknown_kind").is_none());
    let typescript =
        SyntaxNames::new(&ShippedLanguage::TypeScript.language()).expect("TypeScript names");
    assert_eq!(typescript.symbol_kind("interface"), Some("interface"));
}

#[test]
fn markdown_collections_source_and_depth_bounds_are_enforced() {
    let text =
        "# Beacon\n\nSee [Client](guide.md#client) and `Client`.\n\n## Client\n\nOpen a file.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let mut original = markdown_parts(document.markdown_facts().expect("Markdown facts"));
    original.error_ranges.push(ByteRange { start: 0, end: 1 });
    original.omitted_ranges.push(ByteRange { start: 0, end: 1 });
    let restored = MarkdownFacts::from_parts(
        text,
        document.symbols(),
        SyntaxLimits::default(),
        original.clone(),
    )
    .expect("complete facts with errors and omissions");
    assert_eq!(restored.error_ranges(), original.error_ranges);
    assert_eq!(restored.omitted_ranges(), original.omitted_ranges);
    let limit = [
        original.blocks.len(),
        original.headings.len(),
        original.links.len(),
        original.reference_candidates.len(),
    ]
    .into_iter()
    .max()
    .expect("collection lengths");
    let limits = SyntaxLimits::new(text.len(), limit, 8).expect("accepted limits");
    for collection in 0..6 {
        let mut invalid = original.clone();
        match collection {
            0 => invalid.blocks.resize(limit + 1, original.blocks[0].clone()),
            1 => invalid.headings.resize(limit + 1, original.headings[0]),
            2 => invalid.links.resize(limit + 1, original.links[0].clone()),
            3 => invalid
                .reference_candidates
                .resize(limit + 1, original.reference_candidates[0]),
            4 => invalid
                .error_ranges
                .resize(limit + 1, original.error_ranges[0]),
            5 => invalid
                .omitted_ranges
                .resize(limit + 1, original.omitted_ranges[0]),
            _ => unreachable!("six Markdown collections"),
        }
        assert_eq!(
            MarkdownFacts::from_parts(text, document.symbols(), limits, invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_count_exceeded::SLUG)
        );
    }
    assert_eq!(
        MarkdownFacts::from_parts(
            text,
            document.symbols(),
            SyntaxLimits::new(text.len() - 1, 100, 8).expect("source bound"),
            original.clone()
        )
        .map_err(|error| error.slug()),
        Err(errors::syntax::facts_source_too_large::SLUG)
    );
    assert_eq!(
        MarkdownFacts::from_parts(
            text,
            document.symbols(),
            SyntaxLimits::new(text.len(), 100, 1).expect("depth bound"),
            original
        )
        .map_err(|error| error.slug()),
        Err(errors::syntax::facts_depth_exceeded::SLUG)
    );
}

#[test]
fn markdown_headings_must_match_declarations_and_precede_their_blocks() {
    let text = "# Beacon\n\nOpen a file.\n\n## Client\n\nClose a file.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let original = markdown_parts(document.markdown_facts().expect("Markdown facts"));
    for field in 0..3 {
        let mut invalid = original.clone();
        let mut symbols = document.symbols().to_vec();
        match field {
            0 => symbols[invalid.headings[0].symbol_index].kind = "function",
            1 => {
                symbols[invalid.headings[0].symbol_index].range = ByteRange { start: 0, end: 0 };
            }
            2 => invalid.headings[0].level = 0,
            _ => unreachable!("three heading fields"),
        }
        assert_eq!(
            MarkdownFacts::from_parts(text, &symbols, SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_structure_invalid::SLUG),
            "field {field}"
        );
    }
    for heading in [usize::MAX, 1] {
        let mut invalid = original.clone();
        invalid.blocks[0].heading = Some(heading);
        assert_eq!(
            MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_reference_invalid::SLUG)
        );
    }
}

#[test]
fn markdown_candidates_and_link_fields_need_containing_ranges() {
    let text = "# Beacon\n\nSee [Client](guide.md#client) and `Client`.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let original = markdown_parts(document.markdown_facts().expect("Markdown facts"));
    let mut unordered = original.clone();
    let mut later = unordered.reference_candidates[0];
    later.range.start += 1;
    unordered.reference_candidates.insert(0, later);
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), unordered)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_order_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.reference_candidates[0].block_range = ByteRange { start: 0, end: 0 };
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    let mut invalid = original.clone();
    invalid.reference_candidates[0].block_range = original.blocks[0].range;
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG)
    );
    for field in 0..3 {
        let mut invalid = original.clone();
        let outside = Some(ByteRange { start: 0, end: 1 });
        match field {
            0 => invalid.links[0].destination_range = outside,
            1 => invalid.links[0].fragment_range = outside,
            2 => invalid.links[0].label_range = outside,
            _ => unreachable!("three link fields"),
        }
        assert_eq!(
            MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_range_invalid::SLUG),
            "field {field}"
        );
    }
}

#[test]
fn markdown_blocks_before_first_heading_keep_no_heading_context() {
    let text = "Open a file.\n\n# Beacon\n\nClose a file.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let original = document.markdown_facts().expect("Markdown facts");
    assert_eq!(original.blocks()[0].heading, None);
    let restored = MarkdownFacts::from_parts(
        text,
        document.symbols(),
        SyntaxLimits::default(),
        markdown_parts(original),
    )
    .expect("prose before first heading");
    assert_eq!(&restored, original);
}

#[test]
fn markdown_omitted_ranges_preserve_source_order() {
    let text = "# Beacon\n\nOpen a file.\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let mut original = markdown_parts(document.markdown_facts().expect("Markdown facts"));
    original.omitted_ranges = vec![
        ByteRange { start: 0, end: 1 },
        ByteRange { start: 2, end: 3 },
    ];
    let restored = MarkdownFacts::from_parts(
        text,
        document.symbols(),
        SyntaxLimits::default(),
        original.clone(),
    )
    .expect("ordered omitted ranges");
    assert_eq!(restored.omitted_ranges(), original.omitted_ranges);
    original.omitted_ranges.reverse();
    assert_eq!(
        MarkdownFacts::from_parts(text, document.symbols(), SyntaxLimits::default(), original)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_order_invalid::SLUG)
    );
}

#[test]
fn markdown_facts_are_required_only_for_markdown_language() {
    let text = "# Beacon\n";
    let markdown = parsed(ShippedLanguage::Markdown, text);
    let mut missing = parts(markdown.facts());
    missing.markdown_facts = None;
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), missing)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_structure_invalid::SLUG)
    );
    let rust_text = "pub fn open() {}\n";
    let rust = parsed(ShippedLanguage::Rust, rust_text);
    let mut invalid = parts(rust.facts());
    invalid.markdown_facts = markdown.markdown_facts().cloned();
    assert_eq!(
        SyntaxFacts::from_parts(rust_text, SyntaxLimits::default(), invalid)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_structure_invalid::SLUG)
    );
}

#[test]
fn restored_declaration_text_uses_source_bound() {
    let text = "/// Open a file.\npub fn run() {}\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let limits = SyntaxLimits::new(text.len(), 100, 8).expect("source limits");
    for field in 0..3 {
        let mut invalid = original.clone();
        let oversized = "x".repeat(text.len() + 1);
        match field {
            0 => invalid.symbols[0].visibility = Some(oversized),
            1 => {
                let mut docs = invalid.symbols[0].documentation.to_vec();
                docs[0].text = oversized;
                invalid.symbols[0].documentation = docs.into();
            }
            2 => {
                let mut signatures = invalid.symbols[0].signatures.to_vec();
                signatures[0].display = oversized;
                invalid.symbols[0].signatures = signatures.into();
            }
            _ => unreachable!("three text fields"),
        }
        assert_eq!(
            SyntaxFacts::from_parts(text, limits, invalid).map_err(|error| error.slug()),
            Err(errors::syntax::facts_text_exceeded::SLUG)
        );
    }
}

#[test]
fn restored_signatures_keep_syntax_shape() {
    let text = "pub fn run() {}\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    let limits = SyntaxLimits::new(text.len(), 100, 8).expect("source limits");
    for (field, value) in [
        ("language", serde_json::json!("python")),
        (
            "links",
            serde_json::json!([{"range":{"start":0,"end":1},"symbol":"rift://symbol/rust/lib.rs/run"}]),
        ),
        (
            "receiver",
            serde_json::json!({"optional":false,"variadic":false}),
        ),
        (
            "parameters",
            serde_json::json!([{"optional":false,"variadic":false}]),
        ),
        (
            "returns",
            serde_json::json!([{"role":"return","origin":"declared","type":{"language":"rust","source":"u8"}}]),
        ),
        (
            "type_parameters",
            serde_json::json!(["rift://symbol/rust/lib.rs/run"]),
        ),
        (
            "throws",
            serde_json::json!([{"language":"rust","source":"u8"}]),
        ),
        ("effects", serde_json::json!(["async"])),
        (
            "extensions",
            serde_json::json!({"org.rift.fixture":{"version":1,"data":{"value":"x"}}}),
        ),
    ] {
        let mut invalid = original.clone();
        let mut encoded =
            serde_json::to_value(&original.symbols[0].signatures[0]).expect("signature fixture");
        encoded[field] = value;
        let signature = serde_json::from_value(encoded).expect("populated signature fixture");
        invalid.symbols[0].signatures = vec![signature].into();
        assert_eq!(
            SyntaxFacts::from_parts(text, limits, invalid).map_err(|error| error.slug()),
            Err(errors::syntax::facts_structure_invalid::SLUG),
            "{field}"
        );
    }
}

#[test]
fn restored_markdown_code_language_uses_source_bound() {
    let text = "```rust\nfn run() {}\n```\n";
    let document = parsed(ShippedLanguage::Markdown, text);
    let mut invalid = markdown_parts(document.markdown_facts().expect("Markdown facts"));
    invalid.blocks[0].code_language = Some("x".repeat(text.len() + 1));
    assert_eq!(
        MarkdownFacts::from_parts(
            text,
            document.symbols(),
            SyntaxLimits::new(text.len(), 100, 8).expect("source bound"),
            invalid
        )
        .map_err(|error| error.slug()),
        Err(errors::syntax::facts_text_exceeded::SLUG)
    );
}
