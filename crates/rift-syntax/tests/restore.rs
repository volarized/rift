//! Captured provider facts retain their values through checked construction.

use rift_core::ProjectPath;
use rift_error::errors;
use rift_syntax::{
    ByteRange, MarkdownFacts, MarkdownFactsParts, ShippedLanguage, StaticBindingKind, StaticFacts,
    StaticFactsParts, StaticMemberAlias, StaticMemberCopy, SyntaxDocument, SyntaxFacts,
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
        origin: facts.origin(),
        language: facts.language().clone(),
        symbols: facts.symbols().to_vec(),
        left_out_symbols: facts.left_out_symbols().to_vec(),
        has_errors: facts.has_errors(),
        variance_errors_only: facts.variance_errors_only(),
        left_out_declarations: facts.left_out_declaration_count(),
        markdown_facts: facts.markdown_facts().cloned(),
        export_bindings: facts.export_bindings().map(<[_]>::to_vec),
        macro_invocations: facts.macro_invocations().map(<[_]>::to_vec),
        scope: facts.scope().map(<[_]>::to_vec),
        static_facts: facts.static_facts().cloned(),
        source_digest: *facts.source_digest().expect("source witness"),
    }
}

#[test]
fn test_callable_member_alias_restores_only_against_its_inner_assignment() {
    let source = "var req = {};
req.get = req.header = function header() { return 1; };
req.other = function other() { return 2; };
module.exports = req;
";
    let document = parsed(ShippedLanguage::JavaScript, source);
    let captured = parts(document.facts());
    let facts = captured.static_facts.as_ref().expect("static facts");
    let [alias] = facts.member_aliases() else {
        panic!("one captured callable alias");
    };
    let alias = *alias;
    let symbol = |name| {
        document
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == name)
            .expect("symbol")
    };
    assert_eq!(symbol("req.get").name_range, Some(alias.name));
    assert_eq!(symbol("req.header").name_range, Some(alias.target));
    assert!(symbol("req.get").body_range.is_none());
    assert!(symbol("req.header").body_range.is_some());

    let limits = SyntaxLimits::default();
    let restored =
        SyntaxFacts::from_parts(source, limits, captured.clone()).expect("checked alias");
    assert_eq!(restored.static_facts(), Some(facts));
    let mismatched = StaticFactsParts {
        member_aliases: vec![StaticMemberAlias {
            name: alias.name,
            target: symbol("req.other").name_range.expect("other name"),
        }],
        ..StaticFactsParts::default()
    };
    let mut invalid = captured.clone();
    invalid.static_facts =
        Some(StaticFacts::from_parts(source, limits, mismatched).expect("valid ranges"));
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, invalid).map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG),
    );

    let mut older = captured;
    older
        .symbols
        .retain(|symbol| symbol.name_range != Some(alias.name));
    older.static_facts = Some(StaticFacts::default());
    let older = SyntaxFacts::from_parts(source, limits, older).expect("older facts");
    assert!(
        older
            .static_facts()
            .expect("older static facts")
            .member_aliases()
            .is_empty()
    );
}

#[test]
fn test_commonjs_synchronization_keeps_named_exports_after_the_shared_write() {
    let source = "var req = require('./request');
var res = require('./response');
exports.dropped = req;
exports = module.exports = create;
function create() {}
exports.request = req;
exports.response = res;
";
    let get_bindings = |source: &str| {
        let document = parsed(ShippedLanguage::JavaScript, source);
        let captured = parts(document.facts());
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured)
            .expect("checked export capture")
    };
    let checked = get_bindings(source);
    let bindings = checked.export_bindings().expect("captured export bindings");
    let text = |range: ByteRange| {
        &source
            [usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end")]
    };
    let witness = checked
        .static_facts()
        .and_then(rift_syntax::StaticFacts::commonjs_export)
        .expect("closed synchronized write");
    assert!(witness.synchronized);
    assert_eq!(text(witness.assignment), "module.exports = create");
    assert_eq!(
        witness
            .initialized
            .iter()
            .copied()
            .map(text)
            .collect::<Vec<_>>(),
        ["req", "res", "create"]
    );
    let mut removed = parts(&checked);
    removed.export_bindings = Some(Vec::new());
    assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), removed).is_err());
    assert_eq!(
        bindings
            .iter()
            .map(|binding| (
                binding.kind,
                binding.exported.map(text),
                binding.local.map(text)
            ))
            .collect::<Vec<_>>(),
        [
            (rift_syntax::SyntaxExportKind::Default, None, Some("create")),
            (
                rift_syntax::SyntaxExportKind::Named,
                Some("request"),
                Some("req")
            ),
            (
                rift_syntax::SyntaxExportKind::Named,
                Some("response"),
                Some("res")
            ),
        ]
    );
    for changed in [
        "module.exports = req;",
        "exports = req;",
        "exports[slot] = req;",
        "module['exports'] = req;",
        "Object.defineProperty(module, 'exports', { value: req });",
        "function hidden(exports) { exports.response = req; }",
        "function detach() { eval('exports = {}'); } detach();",
    ] {
        let changed = format!("{source}{changed}");
        assert!(
            get_bindings(&changed)
                .export_bindings()
                .expect("captured export bindings")
                .is_empty(),
            "{changed}"
        );
    }
    let non_direct = source.replace(
        "exports = module.exports = create;",
        "if (enabled) exports = module.exports = create;",
    );
    assert!(
        get_bindings(&non_direct)
            .export_bindings()
            .expect("captured export bindings")
            .is_empty()
    );
    let continued = source.replace("exports.request = req;", "exports.request = req\n.member;");
    let continued = get_bindings(&continued);
    assert_eq!(
        continued
            .export_bindings()
            .expect("captured export bindings")
            .len(),
        2
    );
}

#[test]
fn test_commonjs_direct_export_witness_refuses_conditional_and_wrapper_escapes() {
    let target = "var root = {}; module.exports = root;";
    let checked = |source: &str| {
        let document = parsed(ShippedLanguage::JavaScript, source);
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
            .expect("checked export capture")
            .static_facts()
            .and_then(rift_syntax::StaticFacts::commonjs_export)
            .cloned()
    };
    let witness = checked(target).expect("closed direct default write");
    assert!(!witness.synchronized);
    assert_eq!(
        &target[usize::try_from(witness.assignment.start).expect("start")
            ..usize::try_from(witness.assignment.end).expect("end")],
        "module.exports = root"
    );
    for source in [
        "var root = {}; if (flag) module.exports = root;",
        "var root = {}; module.exports = root; getAlias(module);",
        "var root = {}; module.exports = root; module.exports = other;",
        r"var root = {}; module.exports = root; \u006dodule.exports = other;",
    ] {
        assert_eq!(checked(source), None, "{source}");
    }
}

#[test]
fn test_member_copy_checks_original_ranges_and_keeps_older_facts_unknown() {
    let source = "class Owner {
        handle = () => '';
        copy() {
            const clone = new Owner()
            let mutable = new Owner()
            clone.handle = this.handle
            mutable.handle = this.handle
                .bind(this)
        }
    }";
    let document = parsed(ShippedLanguage::TypeScript, source);
    let captured = parts(document.facts());
    let facts = captured.static_facts.as_ref().expect("static facts");
    let [copy] = facts.member_copies() else {
        panic!("only the complete direct member copy");
    };
    let text = |range: ByteRange| {
        &source
            [usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end")]
    };
    assert_eq!(text(copy.assignment), "clone.handle = this.handle");
    assert_eq!(text(copy.target), "clone.handle");
    assert_eq!(text(copy.value), "this.handle");
    assert!(facts.bindings().iter().any(|binding| {
        text(binding.name) == "clone"
            && matches!(binding.kind, StaticBindingKind::JavaScriptConstInstance { constructor }
                if text(constructor) == "Owner")
    }));
    assert!(facts.bindings().iter().any(|binding| {
        text(binding.name) == "mutable"
            && matches!(binding.kind, StaticBindingKind::JavaScriptInstance { constructor }
                if text(constructor) == "Owner")
    }));
    let limits = SyntaxLimits::default();
    let checked = SyntaxFacts::from_parts(source, limits, captured.clone()).expect("checked copy");
    assert_eq!(checked.static_facts(), Some(facts));

    let older = StaticFactsParts {
        bindings: facts
            .bindings()
            .iter()
            .map(|binding| {
                let mut older = *binding;
                if let StaticBindingKind::JavaScriptConstInstance { constructor } = older.kind {
                    older.kind = StaticBindingKind::JavaScriptInstance { constructor };
                }
                older
            })
            .collect(),
        assignments: facts.assignments().to_vec(),
        members: facts.members().to_vec(),
        ..StaticFactsParts::default()
    };
    let mut older_capture = captured.clone();
    older_capture.static_facts = Some(
        StaticFacts::from_parts(source, limits, older).expect("older facts without copy evidence"),
    );
    let older =
        SyntaxFacts::from_parts(source, limits, older_capture).expect("checked older facts");
    let older = older.static_facts().expect("older static facts");
    assert!(older.member_copies().is_empty());
    assert_eq!(older.assignments(), facts.assignments());
    assert_eq!(older.members(), facts.members());

    let partial = StaticFactsParts {
        member_copies: vec![StaticMemberCopy {
            value: ByteRange {
                start: copy.value.start,
                end: copy.value.end - 1,
            },
            ..*copy
        }],
        ..StaticFactsParts::default()
    };
    assert_eq!(
        StaticFacts::from_parts(source, limits, partial).map_err(|error| error.slug()),
        Err(errors::syntax::facts_range_invalid::SLUG),
    );
    let excess = StaticFactsParts {
        member_copies: vec![*copy; 2],
        ..StaticFactsParts::default()
    };
    let limits = SyntaxLimits::new(source.len(), 1, 32).expect("positive limits");
    assert_eq!(
        StaticFacts::from_parts(source, limits, excess).map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG),
    );
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
fn tsx_callable_variable_restores_its_recursive_and_outer_bindings() {
    let source = "export const Widget = () => <div />;";
    let document = parsed(ShippedLanguage::TypeScriptTsx, source);
    let restored =
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
            .expect("restore the provider's Widget bindings");
    assert_eq!(&restored, document.facts());

    let bindings = restored.static_facts().expect("TSX facts").bindings();
    let name = ByteRange { start: 13, end: 19 };
    let widget = bindings
        .iter()
        .filter(|binding| binding.name == name)
        .map(|binding| (binding.scope, binding.available_from))
        .collect::<Vec<_>>();
    assert_eq!(widget.len(), 2);
    assert!(widget.contains(&(ByteRange { start: 0, end: 36 }, 35)));
    assert!(widget.contains(&(ByteRange { start: 13, end: 35 }, 22)));
}

#[test]
fn rust_module_path_facts_restore_exact_source_and_keep_old_facts_unknown() {
    let source = "#[path = \"client.rs\"]\n\nmod client;\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    assert!(matches!(
        captured.symbols[0].module_path,
        Some(rift_syntax::RustModulePath::Literal { .. })
    ));
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("exact module path facts");
    assert_eq!(&restored, document.facts());

    let mut old = captured.clone();
    old.symbols[0].module_path = None;
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), old)
        .expect("older unrecorded module path");
    assert!(restored.symbols()[0].module_path.is_none());

    for mutation in 0..4 {
        let mut invalid = captured.clone();
        let symbol = &mut invalid.symbols[0];
        let Some(rift_syntax::RustModulePath::Literal { path, range }) = &mut symbol.module_path
        else {
            panic!("literal module path");
        };
        match mutation {
            0 => *path = ProjectPath::new("other.rs").expect("different path"),
            1 => range.end += 1,
            2 => *range = symbol.item_range,
            _ => symbol.kind = "function",
        }
        assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err());
    }
}

#[test]
fn python_overload_import_and_decorator_ranges_restore_without_inventing_old_facts() {
    let source = "from typing import overload as ov\n@ov\ndef call(value: int): ...\n";
    let document = parsed(ShippedLanguage::Python, source);
    let captured = parts(document.facts());
    let position = captured
        .symbols
        .iter()
        .position(|symbol| symbol.name == "call")
        .expect("captured function");
    assert!(matches!(
        captured.symbols[position].python_overload,
        Some(rift_syntax::PythonOverload::Overload { .. })
    ));
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("exact original import and decorator");
    assert_eq!(&restored, document.facts());
    let mut old = captured.clone();
    old.symbols[position].python_overload = None;
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), old)
        .expect("unrecorded older overload state");
    assert!(restored.symbols()[position].python_overload.is_none());
    for mutation in 0..5 {
        let mut invalid = captured.clone();
        let symbol = &mut invalid.symbols[position];
        let Some(rift_syntax::PythonOverload::Overload {
            import_statement,
            module,
            imported,
            binding,
            decorator,
        }) = &mut symbol.python_overload
        else {
            panic!("captured overload");
        };
        match mutation {
            0 => *module = *binding,
            1 => *imported = Some(*binding),
            2 => *decorator = symbol.item_range,
            3 => import_statement.end = symbol.item_range.end,
            _ => symbol.kind = "class",
        }
        assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err());
    }
}

#[test]
fn export_binding_restore_preserves_alias_source_and_refuses_invalid_facts() {
    let source = "export { local as exposed } from 'client';\n";
    let document = parsed(ShippedLanguage::JavaScript, source);
    let token = |name: &str| {
        let start = source.find(name).expect("source token");
        ByteRange {
            start: u64::try_from(start).expect("start"),
            end: u64::try_from(start + name.len()).expect("end"),
        }
    };
    let binding = rift_syntax::SyntaxExportBinding {
        kind: rift_syntax::SyntaxExportKind::Named,
        range: ByteRange {
            start: 0,
            end: u64::try_from(source.trim_end().len()).expect("statement"),
        },
        local: Some(token("local")),
        exported: Some(token("exposed")),
        source: Some(token("'client'")),
        container: None,
        type_only: false,
    };
    let mut captured = parts(document.facts());
    captured.export_bindings = Some(vec![binding.clone()]);
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("captured alias binding");
    assert_eq!(
        restored.export_bindings(),
        Some([binding.clone()].as_slice())
    );
    for mutation in 0..5 {
        let mut invalid = captured.clone();
        let facts = invalid.export_bindings.as_mut().expect("recorded bindings");
        match mutation {
            0 => facts[0].local = Some(ByteRange { start: 0, end: 0 }),
            1 => facts[0].exported = None,
            2 => facts[0].container = Some("absent".to_owned()),
            3 => facts[0].range.end = 1,
            _ => facts.push(binding.clone()),
        }
        assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err());
    }
    let mut invalid = captured.clone();
    invalid.language = ShippedLanguage::Rust.language();
    assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err());
    let mut overbound = captured;
    overbound.export_bindings = Some(vec![binding; 2]);
    let limits = SyntaxLimits::new(source.len(), 1, 1).expect("positive bounds");
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, overbound)
            .expect_err("export count bound")
            .slug()
            .as_str(),
        "rift.syntax.facts_count_exceeded"
    );
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
        (
            ShippedLanguage::Html,
            "<main id=\"client\"><script>function open() { return 1; }</script><style>.client { color: blue; }</style></main>",
        ),
        (
            ShippedLanguage::Css,
            ".client { --accent: blue; color: var(--accent); }",
        ),
        (
            ShippedLanguage::C,
            "struct Client { int port; }; int open(void) { return 1; }",
        ),
        (
            ShippedLanguage::Cpp,
            "namespace client { class Client { int open() { return 1; } }; }",
        ),
        (
            ShippedLanguage::Cython,
            "cdef class Client:\n    cpdef int open(self):\n        return 1\n",
        ),
        (
            ShippedLanguage::Jsonc,
            "// client settings\n{\"client\":{\"port\":8080}}",
        ),
        (
            ShippedLanguage::HtmlAngular,
            "@if (client) { <button (click)=\"open()\">{{ client | titlecase }}</button> }",
        ),
        (
            ShippedLanguage::Vue,
            "<script setup lang=\"ts\">function open(): number { return 1; }</script><template><button @click=\"open()\">Client</button></template><style>.client { color: blue; }</style>",
        ),
        (
            ShippedLanguage::Svelte,
            "<script lang=\"ts\">function open(): number { return 1; }</script>{#if client}<button on:click={open}>Client</button>{/if}<style>.client { color: blue; }</style>",
        ),
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
            if let Some(kind) = symbol.syntax_element_kind {
                let resolved_syntax_element = {
                    let decoded_syntax_element = kind.to_owned();
                    names.syntax_element_kind(&decoded_syntax_element)
                };
                assert_eq!(resolved_syntax_element, Some(kind));
            }
        }
    }
}

/// Facts for members, bound names, attributes, and decorated declarations
/// restore exactly, attached documentation included.
#[test]
fn member_and_bound_name_facts_restore_for_every_extended_language() {
    let cases = [
        (
            ShippedLanguage::Rust,
            "pub trait Shape { type Unit; const SIDES: u32; fn area(&self) -> f64; }\n\
             pub struct Point {\n    /// Horizontal.\n    pub x: i32,\n}\n\
             pub union Bits { pub i: u32 }\n\
             impl Shape for Remote { type Unit = i32; const SIDES: u32 = 0; fn area(&self) -> f64 { 1.0 } }\n\
             pub fn a() { fn inner() {} }\n",
        ),
        (
            ShippedLanguage::JavaScript,
            "class Widget {\n  count = 0;\n  #secret = 1;\n  /** Runs. */\n  @bound run() {}\n}\n\
             export const { major, minor: mi } = version;\n\
             export default class { anon() {} }\n",
        ),
        (
            ShippedLanguage::TypeScript,
            "/** Sealed. */\n@sealed\nexport abstract class Shape {\n  /** Side count. */\n  sides: number = 0;\n  \
             abstract area(): number;\n  /** Draws. */\n  @log\n  draw(): void {}\n  \
             constructor(public readonly name: string) {}\n}\n\
             export enum Kind { A = 1, B }\n",
        ),
        (
            ShippedLanguage::Python,
            "LIMIT = 10\n\"\"\"Most rows per page.\"\"\"\nA, B = 1, 2\ntype Pair = tuple[int, int]\n\
             if True:\n    FLAG = 1\n\
             class Model:\n    kind = 'model'\n    \"The model kind.\"\n    \
             @property\n    def label(self):\n        return 1\n    \
             def __init__(self):\n        self.name = 'x'\n        \"The display name.\"\n\
             def keys(d):\n    pass\n\nkeys.__doc__ = \"Return keys.\"\n",
        ),
    ];
    for (shipped, text) in cases {
        let document = parsed(shipped, text);
        assert!(!document.has_errors(), "{shipped:?}");
        assert!(
            document
                .symbols()
                .iter()
                .any(|symbol| !symbol.documentation.is_empty()),
            "{shipped:?} fixture attaches documentation"
        );
        let restored =
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
                .unwrap_or_else(|error| panic!("{shipped:?} facts restore: {error:?}"));
        assert_eq!(&restored, document.facts(), "{shipped:?}");
    }
}

#[test]
fn declaration_ordinals_parser_errors_and_omissions_are_preserved() {
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
fn declaration_ordinals_out_of_source_order_are_refused() {
    let text = "pub fn open() {}\npub fn open() {}\n";
    let document = parsed(ShippedLanguage::Rust, text);
    let original = parts(document.facts());
    for ordinals in [[0, 0], [1, 0], [0, 2]] {
        let mut numbered = original.clone();
        for (symbol, ordinal) in numbered.symbols.iter_mut().zip(ordinals) {
            symbol.declaration_ordinal = ordinal;
        }
        assert_eq!(
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), numbered)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_name_invalid::SLUG),
            "{ordinals:?}"
        );
    }
}

#[test]
fn a_repeated_container_resolves_to_the_declaration_enclosing_each_child() {
    let text = "{\"servers\": {\"host\": 1}, \"servers\": {\"host\": 2}}";
    let document = parsed(ShippedLanguage::Json, text);
    let keys = document
        .symbols()
        .iter()
        .map(|symbol| (symbol.qualified_name.as_str(), symbol.declaration_ordinal))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            ("servers", 0),
            ("servers > host", 0),
            ("servers", 1),
            ("servers > host", 1)
        ]
    );
    let restored = SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
        .expect("each child sits inside its own container");
    assert_eq!(&restored, document.facts());
    let mut shrunk = parts(document.facts());
    let second = text.rfind("\"servers\"").expect("the second key") as u64;
    let key = ByteRange {
        start: second,
        end: second + "\"servers\"".len() as u64,
    };
    let container = &mut shrunk.symbols[2];
    container.range = key;
    container.item_range = key;
    container.body_range = None;
    container.documentation_ranges.clear();
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), shrunk)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG),
        "a child its container's latest earlier declaration does not enclose is refused"
    );
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
    invalid.symbols[0].syntax_element_kind = Some("unknown_syntax_element_kind");
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
fn a_member_assigned_after_its_owner_restores_with_the_owner_as_container() {
    let text = "function logger() {}\nlogger.warn = () => {};\nPage.load = function () {};\nfunction Page() {}\n";
    let document = parsed(ShippedLanguage::JavaScript, text);
    let containers = document
        .symbols()
        .iter()
        .map(|symbol| (symbol.qualified_name.as_str(), symbol.container.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        containers,
        [
            ("logger", None),
            ("logger.warn", Some("logger")),
            ("Page.load", Some("Page")),
            ("Page", None)
        ]
    );
    let restored = SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
        .expect("an assigned member names its owner by binding, not by nesting");
    assert_eq!(&restored, document.facts());

    let mut nested = parts(document.facts());
    nested.symbols[3].syntax_element_kind = Some("assignment_expression");
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), nested)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG),
        "an assigned member's container is a declaration not itself assigned"
    );
    let mut unassigned = parts(document.facts());
    unassigned.symbols[1].syntax_element_kind = Some("function_declaration");
    assert_eq!(
        SyntaxFacts::from_parts(text, SyntaxLimits::default(), unassigned)
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_reference_invalid::SLUG),
        "any other declaration still nests inside its container"
    );
    let limits = SyntaxLimits::new(text.len(), 100, 1).expect("bounds");
    assert_eq!(
        SyntaxFacts::from_parts(text, limits, parts(document.facts()))
            .map_err(|error| error.slug()),
        Err(errors::syntax::facts_depth_exceeded::SLUG),
        "an assigned member sits one level below its owner"
    );
}

#[test]
fn a_member_assigned_inside_its_own_method_keeps_the_method_as_container() {
    let text = "class Stream {\n  run() {\n    this.run = null;\n    const step = 1;\n  }\n}\n";
    let document = parsed(ShippedLanguage::JavaScript, text);
    let containers = document
        .symbols()
        .iter()
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.kind,
                symbol.container.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        containers,
        [
            ("Stream", "class", None),
            ("Stream.run", "method", Some("Stream")),
            ("Stream.run", "property", Some("Stream")),
            ("Stream.run.step", "variable", Some("Stream.run")),
        ]
    );
    let restored = SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(document.facts()))
        .expect("the method, not the facet assigned inside it, holds the later declaration");
    assert_eq!(&restored, document.facts());
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
    assert!(javascript.syntax_element_kind("unknown_kind").is_none());
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
    let identity = rift_protocol::identity::SymbolIdentity::new(
        rift_protocol::identity::SymbolOwner::Local,
        ShippedLanguage::Rust.language(),
        vec!["fixture".to_owned(), "run".to_owned()],
    )
    .expect("canonical fixture identity")
    .wire_identity();
    for (field, value) in [
        ("language", serde_json::json!("python")),
        (
            "links",
            serde_json::json!([{"range":{"start":0,"end":1},"symbol":identity}]),
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
        ("type_parameters", serde_json::json!([identity])),
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

#[test]
fn full_omitted_names_restore_with_source_ranges_and_parent_validation() {
    for characters in [3000, 4097] {
        let name = "𐐀".repeat(characters);
        let source = format!(
            "/// Retained original declaration.\npub fn {name}(value: u8) -> u8 {{ value }}\npub fn finish() {{}}\n"
        );
        let document = parsed(ShippedLanguage::Rust, &source);
        assert!(!document.has_errors());
        assert!(name.len() > rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX);
        let original = document.facts();
        assert_eq!(original.symbols().len(), 1);
        assert_eq!(original.symbols()[0].name, "finish");
        assert_eq!(original.left_out_symbols().len(), 1);
        assert_eq!(original.left_out_declaration_count(), 1);
        let omitted = &original.left_out_symbols()[0];
        assert_eq!(omitted.name, name);
        let start = usize::try_from(omitted.item_range.start).expect("item start");
        let end = usize::try_from(omitted.item_range.end).expect("item end");
        assert!(source[start..end].contains(&name));
        assert_eq!(omitted.signatures.len(), 1);
        assert_eq!(
            omitted.documentation[0].text,
            "Retained original declaration."
        );
        let restored = SyntaxFacts::from_parts(&source, SyntaxLimits::default(), parts(original))
            .expect("full omitted facts restore");
        assert_eq!(&restored, original);
        let mut invalid = parts(original);
        invalid.left_out_declarations = 0;
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), invalid)
                .expect_err("omitted inventory count")
                .slug()
                .as_str(),
            "rift.syntax.facts_count_exceeded"
        );
        let mut invalid = parts(original);
        invalid.left_out_symbols[0].container = Some("finish".to_owned());
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), invalid)
                .expect_err("parent occurs after omitted declaration")
                .slug()
                .as_str(),
            "rift.syntax.facts_reference_invalid"
        );
    }
}

#[test]
fn retained_namespace_export_bindings_resolve_original_container_facts() {
    let name = "𐐀".repeat(3000);
    let source = format!(
        "declare module \"{name}\" {{ export function open(): void; }}\nexport function finish(): void;\n"
    );
    let document = parsed(ShippedLanguage::TypeScript, &source);
    assert!(!document.has_errors());
    let original = document.facts();
    let bindings = original
        .export_bindings()
        .expect("recorded original exports");
    let container = bindings
        .iter()
        .find_map(|binding| binding.container.as_ref())
        .expect("original ambient namespace container");
    assert!(!rift_core::is_portable_name(container));
    assert!(
        original
            .left_out_symbols()
            .iter()
            .any(|symbol| &symbol.qualified_name == container)
    );
    let restored = SyntaxFacts::from_parts(&source, SyntaxLimits::default(), parts(original))
        .expect("retained container and export bindings restore together");
    assert_eq!(&restored, original);
    let mut invalid = parts(original);
    invalid.export_bindings.as_mut().expect("recorded exports")[0].container =
        Some("absent".to_owned());
    assert_eq!(
        SyntaxFacts::from_parts(&source, SyntaxLimits::default(), invalid)
            .expect_err("missing export container")
            .slug()
            .as_str(),
        "rift.syntax.facts_reference_invalid"
    );
}

#[test]
fn typescript_global_augmentation_exports_restore_under_their_written_scope() {
    let text = "export interface ModuleApi { enabled: boolean; }\ndeclare global {\n    export interface AppState { payload: Payload; }\n    export interface Payload { ready: boolean; }\n}\n";
    for language in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
        let document = parsed(language, text);
        assert!(!document.has_errors());
        let original = document.facts();
        assert_eq!(
            original
                .symbols()
                .iter()
                .map(|symbol| symbol.qualified_name.as_str())
                .collect::<Vec<_>>(),
            [
                "ModuleApi",
                "ModuleApi.enabled",
                "global.AppState",
                "global.AppState.payload",
                "global.Payload",
                "global.Payload.ready",
            ]
        );
        let bindings = original.export_bindings().expect("recorded exports");
        assert_eq!(
            bindings
                .iter()
                .map(|binding| binding.container.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("global"), Some("global")]
        );
        let restored = SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts(original))
            .expect("global augmentation contains both export statements");
        assert_eq!(&restored, original);
        let payload_start = text.find("Payload;").expect("written type");
        let payload_type = ByteRange {
            start: u64::try_from(payload_start).expect("type start"),
            end: u64::try_from(payload_start + "Payload".len()).expect("type end"),
        };
        assert!(
            restored
                .static_facts()
                .expect("static syntax")
                .references()
                .iter()
                .any(|reference| {
                    reference.role == rift_core::ReferenceRole::Type
                        && reference.target == payload_type
                })
        );

        for (binding_index, container) in [(0, "global"), (1, "ModuleApi")] {
            let mut invalid = parts(original);
            invalid.export_bindings.as_mut().expect("exports")[binding_index].container =
                Some(container.to_owned());
            assert_eq!(
                SyntaxFacts::from_parts(text, SyntaxLimits::default(), invalid)
                    .expect_err("an unrelated scope cannot own the export")
                    .slug(),
                errors::syntax::facts_reference_invalid::SLUG
            );
        }
    }
}

#[test]
fn retained_source_whitespace_roundtrips_without_portable_identity() {
    for whitespace in ["\n", "\r", "\t", "\u{000c}"] {
        let source = format!(
            ".first,{whitespace}.second {{ color: red; }}\n.neighbor {{ display: block; }}\n"
        );
        let document = parsed(ShippedLanguage::Css, &source);
        assert!(!document.has_errors());
        let facts = document.facts();
        let retained = facts
            .left_out_symbols()
            .iter()
            .find(|symbol| symbol.kind == "selector")
            .expect("original selector with source whitespace");
        assert!(retained.name.contains(whitespace));
        assert!(!rift_core::is_portable_name(&retained.name));
        assert!(rift_protocol::read::symbol_name_is_retained(
            &retained.name,
            source.len()
        ));
        let restored = SyntaxFacts::from_parts(&source, SyntaxLimits::default(), parts(facts))
            .expect("original retained selectors and child containers restore");
        assert_eq!(&restored, facts);
        assert!(
            restored
                .symbols()
                .iter()
                .any(|symbol| symbol.name == ".neighbor")
        );
        for control in ['\0', '\u{000b}', '\u{007f}'] {
            let mut invalid = parts(facts);
            invalid.left_out_symbols[0].name.push(control);
            assert_eq!(
                SyntaxFacts::from_parts(&source, SyntaxLimits::default(), invalid)
                    .expect_err("non-whitespace controls remain refused")
                    .slug()
                    .as_str(),
                "rift.syntax.facts_name_invalid"
            );
        }
        let mut invalid = parts(facts);
        invalid
            .left_out_symbols
            .iter_mut()
            .find(|symbol| symbol.container.is_some())
            .expect("original child container")
            .container = Some("absent\nparent".to_owned());
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), invalid)
                .expect_err("control-containing missing parent remains refused")
                .slug()
                .as_str(),
            "rift.syntax.facts_name_invalid"
        );
    }
}

#[test]
fn rust_macro_invocations_keep_original_top_level_ranges_and_unknown_state() {
    let source = "macro_rules! cfg_rt { ($($item:item)*) => { $( #[cfg(feature = \"rt\")] $item )* } }\ncfg_rt! { pub mod runtime; }\nmod nested { cfg_rt! { pub mod hidden; } }\npub fn neighbor() {}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let captured = parts(document.facts());
    let invocations = captured
        .macro_invocations
        .as_ref()
        .expect("captured Rust invocations");
    assert_eq!(invocations.len(), 1);
    let invocation = invocations[0].0;
    assert_eq!(
        &source[usize::try_from(invocation.start).expect("start")
            ..usize::try_from(invocation.end).expect("end")],
        "cfg_rt! { pub mod runtime; }"
    );
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("captured macro ranges restore");
    assert_eq!(&restored, document.facts());
    assert!(
        restored
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "neighbor")
    );
    let known_heap = get_size2::GetSize::get_heap_size(&restored);
    let invocation_heap = get_size2::GetSize::get_heap_size(&captured.macro_invocations);
    assert!(
        invocation_heap > 0,
        "captured invocation ranges own material"
    );
    let mut unknown = captured;
    unknown.macro_invocations = None;
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), unknown)
        .expect("old unrecorded ranges remain unknown");
    assert_eq!(restored.macro_invocations(), None);
    assert_eq!(
        known_heap,
        get_size2::GetSize::get_heap_size(&restored).saturating_add(invocation_heap),
        "retained syntax charge includes every captured invocation allocation"
    );
    let plain = parsed(ShippedLanguage::Rust, "pub fn neighbor() {}\n");
    assert_eq!(plain.facts().macro_invocations(), Some([].as_slice()));
    let foreign = parsed(ShippedLanguage::Python, "def neighbor(): pass\n");
    assert_eq!(foreign.facts().macro_invocations(), None);
    for (source, state) in [
        ("#[macro_use]\n#[doc(hidden)]\nmod macros;\n", 0),
        ("#[macro_use]\n#[doc(hidden)]\nmod macros;", 0),
        ("mod macros;", 0),
        ("#[macro_use]\n#[path = \"actual.rs\"]\nmod macros;\n", 1),
        ("#[macro_use]\n#[unknown]\nmod macros;\n", 2),
    ] {
        let document = parsed(ShippedLanguage::Rust, source);
        let symbol = &document.facts().symbols()[0];
        assert_eq!(symbol.syntax_element_kind, Some("mod_item"));
        assert_eq!(symbol.body_range, None);
        assert_eq!(
            &source[usize::try_from(symbol.item_range.start).expect("item start")
                ..usize::try_from(symbol.item_range.end).expect("item end")],
            "mod macros;",
        );
        let path = symbol.module_path.as_ref();
        match state {
            0 => assert_eq!(path, Some(&rift_syntax::RustModulePath::Absent)),
            1 => assert!(matches!(
                path,
                Some(rift_syntax::RustModulePath::Literal { .. })
            )),
            _ => assert_eq!(path, Some(&rift_syntax::RustModulePath::Unknown)),
        }
        let restored =
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
                .expect("captured module attributes restore");
        assert_eq!(&restored, document.facts());
    }
}

#[test]
fn rust_macro_invocation_restore_refuses_invalid_ranges_language_and_count() {
    let source = "// 灯\ncfg_rt! { pub mod runtime; }\ncfg_rt! { pub mod fs; }\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    let ranges = captured
        .macro_invocations
        .as_ref()
        .expect("captured invocations");
    assert_eq!(ranges.len(), 2);
    let first = ranges[0].clone();
    for mutation in 0..6 {
        let mut invalid = captured.clone();
        let ranges = invalid.macro_invocations.as_mut().expect("captured ranges");
        match mutation {
            0 => ranges[0].0.end = ranges[0].0.start,
            1 => ranges[1].0.start = ranges[0].0.end - 1,
            2 => ranges.swap(0, 1),
            3 => ranges[0].0.start = 4,
            4 => ranges[1].0.end = u64::try_from(source.len() + 1).expect("source bound"),
            _ => invalid.language = ShippedLanguage::Python.language(),
        }
        assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err());
    }
    let mut overbound = captured;
    overbound.macro_invocations = Some(vec![first; 2]);
    let limits = SyntaxLimits::new(source.len(), 1, 100).expect("positive limits");
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, overbound).map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG)
    );
}

#[test]
fn rust_macro_direct_tokens_keep_original_ranges_unknown_empty_and_charge() {
    let source = "cfg_rt! { pub mod runtime; use crate::time::Instant; #[derive(Debug)] enum Timer { Value } impl Timer { fn poll<'a>(&'a self) { let x = '}'; let y = '\\\''; let raw = r##\"} fake! {}\"##; 'again: loop { break 'again; } } } }\ncfg_rt! {}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let captured = parts(document.facts());
    let invocations = captured
        .macro_invocations
        .as_ref()
        .expect("captured invocations");
    assert_eq!(invocations.len(), 2);
    let direct = invocations[0].2.as_ref().expect("complete direct tokens");
    let expected = document
        .syntax_elements()
        .iter()
        .filter(|syntax_element| {
            syntax_element
                .parent
                .and_then(|parent| document.syntax_elements().get(parent))
                .is_some_and(|parent| {
                    parent.kind == "token_tree"
                        && parent
                            .parent
                            .and_then(|parent| document.syntax_elements().get(parent))
                            .is_some_and(|parent| parent.range == invocations[0].0)
                })
        })
        .map(|syntax_element| syntax_element.range)
        .collect::<Vec<_>>();
    assert_eq!(direct, &expected);
    assert!(direct.iter().any(|range| &source
        [usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end")]
        == "runtime"));
    assert_eq!(invocations[1].2.as_deref(), Some([].as_slice()));
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("original complete ranges restore");
    assert_eq!(&restored, document.facts());
    let mut unknown = captured;
    let rows = unknown.macro_invocations.as_mut().expect("captured rows");
    let owned = rows[0].2.take().expect("owned direct ranges");
    let charge = get_size2::GetSize::get_heap_size(&Some(owned));
    let unknown = SyntaxFacts::from_parts(source, SyntaxLimits::default(), unknown)
        .expect("old unknown direct ranges remain unknown");
    assert_eq!(unknown.macro_invocations().expect("rows")[0].2, None);
    assert_eq!(
        get_size2::GetSize::get_heap_size(&restored),
        get_size2::GetSize::get_heap_size(&unknown).saturating_add(charge)
    );
}

#[test]
fn rust_macro_direct_tokens_refuse_missing_split_group_and_bound_witnesses() {
    let source = "cfg_rt! { pub mod runtime; use crate::time; impl Timer { fn value() {} } }";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    let direct = captured.macro_invocations.as_ref().expect("rows")[0]
        .2
        .as_ref()
        .expect("complete direct ranges");
    let identifier = direct
        .iter()
        .position(|range| {
            &source[usize::try_from(range.start).expect("start")
                ..usize::try_from(range.end).expect("end")]
                == "runtime"
        })
        .expect("module identifier");
    let group = direct
        .iter()
        .position(|range| source.as_bytes()[usize::try_from(range.start).expect("start")] == b'{')
        .expect("implementation group");
    for mutation in 0..6 {
        let mut invalid = captured.clone();
        let ranges = invalid.macro_invocations.as_mut().expect("rows")[0]
            .2
            .as_mut()
            .expect("direct rows");
        match mutation {
            0 => {
                ranges.remove(identifier);
            }
            1 => {
                ranges.remove(group);
            }
            2 => ranges.clear(),
            3 => ranges[identifier].end -= 1,
            4 => ranges[identifier].start = 0,
            _ => ranges.swap(0, 1),
        }
        assert_eq!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid)
                .map_err(|error| error.slug()),
            Err(if mutation == 5 {
                errors::syntax::facts_order_invalid::SLUG
            } else {
                errors::syntax::facts_range_invalid::SLUG
            })
        );
    }
    let count = captured
        .macro_invocations
        .as_ref()
        .expect("rows")
        .iter()
        .map(|(_, modules, direct)| 1 + modules.len() + direct.as_ref().map_or(0, Vec::len))
        .sum::<usize>();
    let limits = SyntaxLimits::new(source.len(), count - 1, 100).expect("positive limits");
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, captured).map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG)
    );
}

#[test]
fn rust_macro_direct_tokens_distinguish_literals_comments_and_invalid_groups() {
    for body in [
        "impl Timer { fn poll<'a>(&'a self) { 'again: loop { break 'again; } } }",
        "const VALUE: char = '}'; const QUOTE: char = '\\\''; const BYTE: u8 = b'}';",
        "const VALUE: &str = r##\"} /* not comment */ fake! {}\"##;",
        "// named comment\nimpl Timer { /* comment } */ fn value() {} }",
    ] {
        let source = format!("cfg_rt! {{ {body} }}");
        let document = parsed(ShippedLanguage::Rust, &source);
        let captured = parts(document.facts());
        assert!(
            captured.macro_invocations.as_ref().expect("rows")[0]
                .2
                .is_some(),
            "complete original body: {body}"
        );
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), captured)
                .expect("original literal ranges restore"),
            *document.facts()
        );
    }
    let document = parsed(
        ShippedLanguage::Rust,
        "cfg_rt! { impl Timer { fn value() {} }",
    );
    assert!(document.has_errors());
    assert!(
        document
            .facts()
            .macro_invocations()
            .is_some_and(|rows| rows.iter().all(|(_, _, direct)| direct.is_none()))
    );
}

#[test]
fn rust_scope_preserves_original_root_and_module_children() {
    let source = "#![allow(dead_code)]\nextern crate core;\nuse core::fmt;\nmacro_rules! cfg_rt { ($($item:item)*) => { $($item)* }; }\nmod client {\n#[derive(Debug)]\nstruct Client;\nuse super::fmt;\n}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let scopes = document.facts().scope().expect("original Rust scopes");
    assert_eq!(scopes.len(), 2);
    for (range, children) in scopes {
        let (parent, _) = document
            .syntax_elements()
            .iter()
            .enumerate()
            .find(|(_, syntax_element)| {
                syntax_element.range == *range
                    && matches!(syntax_element.kind, "source_file" | "declaration_list")
            })
            .expect("original scope syntax element");
        let original = document
            .syntax_elements()
            .iter()
            .filter(|syntax_element| syntax_element.parent == Some(parent))
            .map(|syntax_element| syntax_element.range)
            .collect::<Vec<_>>();
        assert_eq!(children.as_ref(), Some(&original));
    }
    let restored =
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
            .expect("complete original scopes restore");
    assert_eq!(&restored, document.facts());
}

#[test]
fn rust_scope_omitted_imports_attributes_and_definitions_are_refused() {
    let source = "#![allow(dead_code)]\nextern crate core;\nuse core::fmt;\nmacro_rules! cfg_rt { ($($item:item)*) => { $($item)* }; }\nmod client {}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    let original = captured.scope.as_ref().expect("scopes")[0]
        .1
        .as_ref()
        .expect("root children");
    assert_eq!(original.len(), 5);
    for index in 0..original.len() {
        let mut invalid = captured.clone();
        invalid.scope.as_mut().expect("scopes")[0]
            .1
            .as_mut()
            .expect("root children")
            .remove(index);
        assert_eq!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid)
                .expect_err("omitted original scope effect")
                .slug()
                .as_str(),
            "rift.syntax.facts_range_invalid"
        );
    }
}

#[test]
fn rust_scope_unknown_and_empty_remain_distinct() {
    for source in ["", "mod client {}\n"] {
        let document = parsed(ShippedLanguage::Rust, source);
        let captured = parts(document.facts());
        assert!(captured.scope.as_ref().is_some_and(|rows| !rows.is_empty()));
        assert!(
            captured
                .scope
                .as_ref()
                .expect("scopes")
                .last()
                .expect("scope")
                .1
                .as_ref()
                .is_some_and(Vec::is_empty)
        );
        let mut unknown = captured.clone();
        unknown.scope = None;
        assert!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), unknown)
                .expect("older unknown scope")
                .scope()
                .is_none()
        );
        let mut unknown = captured.clone();
        unknown.scope.as_mut().expect("scopes")[0].1 = None;
        assert!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), unknown)
                .expect("unknown direct children")
                .scope()
                .expect("scope rows")[0]
                .1
                .is_none()
        );
        let mut absent = captured;
        absent.scope = Some(Vec::new());
        assert!(SyntaxFacts::from_parts(source, SyntaxLimits::default(), absent).is_err());
    }
    let source = "mod client { use core::; }";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(document.has_errors());
    let mut invalid = parts(document.facts());
    invalid.scope.as_mut().expect("original scopes")[0].1 = Some(Vec::new());
    assert_eq!(
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid)
            .expect_err("parser errors cannot prove complete scope children")
            .slug()
            .as_str(),
        "rift.syntax.facts_structure_invalid"
    );
    assert!(
        document
            .facts()
            .scope()
            .expect("original scopes")
            .iter()
            .any(|(_, children)| children.is_none())
    );
}

#[test]
fn rust_scope_restore_refuses_wrong_ownership_ranges_and_bounds() {
    let source = "mod client { mod nested {} } fn call() {}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    assert_eq!(captured.scope.as_ref().expect("scopes").len(), 3);
    for mutation in 0..6 {
        let mut invalid = captured.clone();
        let rows = invalid.scope.as_mut().expect("scope rows");
        match mutation {
            0 => rows[0].0.start = 1,
            1 => {
                rows[1].0 = document
                    .symbols()
                    .iter()
                    .find(|symbol| symbol.name == "call")
                    .expect("function")
                    .body_range
                    .expect("function body");
            }
            2 => rows[2].0.end = rows[1].0.end + 1,
            3 => rows[0].1.as_mut().expect("root children")[0].end = u64::MAX,
            4 => rows[0].1.as_mut().expect("root children").reverse(),
            _ => rows[1].1.as_mut().expect("module children")[0].start = 0,
        }
        assert!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid).is_err(),
            "mutation {mutation}"
        );
    }
    let limits = SyntaxLimits::new(source.len(), 1, 100).expect("positive count bound");
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, captured)
            .expect_err("aggregate scope count bound")
            .slug()
            .as_str(),
        "rift.syntax.facts_count_exceeded"
    );
}

#[test]
fn rust_scope_restore_bounds_groups_inside_complete_root_children() {
    let source = "fn call() { let value = (((1))); }\n";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(document.facts().scope().expect("scopes")[0].1.is_some());
    let limits = SyntaxLimits::new(source.len(), 1000, 2).expect("positive depth bound");
    assert!(SyntaxFacts::from_parts(source, limits, parts(document.facts())).is_err());
}

#[test]
fn rust_scope_charge_keeps_owned_rows_and_direct_children() {
    let source = "use core::fmt; mod client { struct Client; }\n";
    let document = parsed(ShippedLanguage::Rust, source);
    let captured = parts(document.facts());
    let known = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("scope material");
    let mut without_children = captured.clone();
    let children_charge = without_children
        .scope
        .as_mut()
        .expect("scope rows")
        .iter_mut()
        .fold(0_usize, |charge, (_, children)| {
            let ranges = children.take().expect("complete children");
            charge.saturating_add(
                ranges
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ByteRange>()),
            )
        });
    let without_children =
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), without_children)
            .expect("unknown child material");
    assert_eq!(
        get_size2::GetSize::get_heap_size(&known),
        get_size2::GetSize::get_heap_size(&without_children).saturating_add(children_charge)
    );
    let mut without_scope = captured;
    let rows = without_scope.scope.take().expect("owned scope rows");
    let scope_charge = rows.iter().fold(
        rows.capacity()
            .saturating_mul(std::mem::size_of::<rift_syntax::Scope>()),
        |charge, (_, children)| {
            charge.saturating_add(children.as_ref().map_or(0, |ranges| {
                ranges
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ByteRange>())
            }))
        },
    );
    let without_scope = SyntaxFacts::from_parts(source, SyntaxLimits::default(), without_scope)
        .expect("unknown scope material");
    assert_eq!(
        get_size2::GetSize::get_heap_size(&known),
        get_size2::GetSize::get_heap_size(&without_scope).saturating_add(scope_charge)
    );
}

#[test]
fn rust_original_token_callback_preserves_literals_lifetimes_and_bounds() {
    let source = "fn call<'a>(value: &'a str) { let character = '\\''; let text = r###\"macro_export! use fake;\"###; /* use hidden; */ nested! { value } }";
    let range = rift_syntax::ByteRange {
        start: 0,
        end: u64::try_from(source.len()).expect("source bound"),
    };
    let mut values = Vec::new();
    assert!(rift_syntax::tokens(
        source,
        range,
        SyntaxLimits::default(),
        |range| {
            let value = source
                .get(
                    usize::try_from(range.start).expect("start")
                        ..usize::try_from(range.end).expect("end"),
                )
                .expect("original token");
            values.push(value);
            true
        }
    ));
    assert!(values.contains(&"'a"));
    assert!(values.contains(&"'\\''"));
    assert!(values.contains(&"r###\"macro_export! use fake;\"###"));
    assert!(!values.contains(&"macro_export"));
    assert!(!values.contains(&"hidden"));
    assert!(values.windows(2).any(|pair| pair == ["nested", "!"]));
    assert!(!rift_syntax::tokens(
        source,
        range,
        SyntaxLimits::default(),
        |_| false
    ));
    for value in [
        "{ /* nested /* comment */ */ }",
        "{ \"unfinished",
        "{ unknown! { }",
    ] {
        let range = rift_syntax::ByteRange {
            start: 0,
            end: u64::try_from(value.len()).expect("source bound"),
        };
        assert!(!rift_syntax::tokens(
            value,
            range,
            SyntaxLimits::default(),
            |_| true
        ));
    }
    assert!(!rift_syntax::tokens(
        source,
        range,
        SyntaxLimits::new(source.len(), 1, 100).expect("count bound"),
        |_| true
    ));
    assert!(!rift_syntax::tokens(
        source,
        range,
        SyntaxLimits::new(source.len(), 1000, 1).expect("depth bound"),
        |_| true
    ));
}

#[test]
fn rust_root_expression_macro_invocations_keep_original_parent_and_restore_charge() {
    let source = "first!(value);\nsecond! { pub mod neighbor; }\nmod nested { inside!(value); }\npub fn body() { inside!(value); }\n";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let captured = parts(document.facts());
    let invocations = captured
        .macro_invocations
        .as_ref()
        .expect("captured root invocations");
    assert_eq!(invocations.len(), 2);
    for (index, expected) in ["first!(value)", "second! { pub mod neighbor; }"]
        .iter()
        .enumerate()
    {
        let range = invocations[index].0;
        assert_eq!(
            &source[usize::try_from(range.start).expect("start")
                ..usize::try_from(range.end).expect("end")],
            *expected
        );
        let original = document
            .syntax_elements()
            .iter()
            .find(|syntax_element| {
                syntax_element.kind == "macro_invocation" && syntax_element.range == range
            })
            .expect("original invocation syntax element");
        let parent = document
            .syntax_elements()
            .get(original.parent.expect("original parent"))
            .expect("parent syntax element");
        if index == 0 {
            assert_eq!(parent.kind, "expression_statement");
            let root = document
                .syntax_elements()
                .get(parent.parent.expect("original root"))
                .expect("root syntax element");
            assert_eq!(root.kind, "source_file");
            assert_eq!(root.parent, None);
        } else {
            assert_eq!(parent.kind, "source_file");
            assert_eq!(parent.parent, None);
        }
        assert!(
            invocations[index].2.is_some(),
            "original direct token ranges remain complete"
        );
    }
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured.clone())
        .expect("original root forms restore");
    assert_eq!(&restored, document.facts());
    let mut unknown = captured.clone();
    let charge = get_size2::GetSize::get_heap_size(&unknown.macro_invocations);
    unknown.macro_invocations = None;
    let unknown = SyntaxFacts::from_parts(source, SyntaxLimits::default(), unknown)
        .expect("old invocation facts remain unknown");
    assert_eq!(unknown.macro_invocations(), None);
    assert_eq!(
        get_size2::GetSize::get_heap_size(&restored),
        get_size2::GetSize::get_heap_size(&unknown).saturating_add(charge)
    );
    let limits = SyntaxLimits::new(source.len(), 1, 100).expect("positive limits");
    assert_eq!(
        SyntaxFacts::from_parts(source, limits, captured).map_err(|error| error.slug()),
        Err(errors::syntax::facts_count_exceeded::SLUG)
    );
}

#[test]
fn rust_macro_doc_comments_keep_original_line_endings_and_refuse_hidden_code() {
    for ending in ["\n", "\r\n"] {
        let source = format!(
            "known! {{{ending}/// original comment{ending}pub mod child;{ending}}}{ending}"
        );
        let document = parsed(ShippedLanguage::Rust, &source);
        assert!(!document.has_errors());
        let mut captured = parts(document.facts());
        let rows = captured
            .macro_invocations
            .as_mut()
            .expect("captured invocation");
        assert_eq!(rows.len(), 1);
        let children = rows[0]
            .2
            .as_mut()
            .expect("original comment retains complete children");
        let comment = children
            .iter()
            .position(|range| {
                source[usize::try_from(range.start).expect("start")
                    ..usize::try_from(range.end).expect("end")]
                    .starts_with("///")
            })
            .expect("original named comment range");
        let range = children[comment];
        assert_eq!(
            &source[usize::try_from(range.start).expect("start")
                ..usize::try_from(range.end).expect("end")],
            format!("/// original comment{ending}")
        );
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), captured.clone())
                .expect("exact comment restores"),
            *document.facts()
        );
        let children = captured
            .macro_invocations
            .as_mut()
            .expect("captured invocation")[0]
            .2
            .as_mut()
            .expect("complete children");
        children[comment].end += 1;
        assert_eq!(
            SyntaxFacts::from_parts(&source, SyntaxLimits::default(), captured)
                .map_err(|error| error.slug()),
            Err(errors::syntax::facts_range_invalid::SLUG),
            "a named comment cannot hide code after its line ending"
        );
    }
    let source = "known! { /* unterminated }";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(document.has_errors());
    assert!(
        document
            .facts()
            .macro_invocations()
            .is_none_or(|rows| rows.iter().all(|(_, _, children)| children.is_none()))
    );
}

#[test]
fn documentation_cfg_module_paths_restore_original_source_and_unknown_effects() {
    for (source, expected) in [
        (
            "#[cfg_attr(docsrs, doc(cfg(feature = \"rt\")))]\npub mod task;",
            rift_syntax::RustModulePath::Absent,
        ),
        (
            "#[cfg_attr(docsrs, doc(hidden), path = \"other.rs\")]\nmod task;",
            rift_syntax::RustModulePath::Unknown,
        ),
    ] {
        let document = parsed(ShippedLanguage::Rust, source);
        assert_eq!(document.symbols()[0].module_path, Some(expected));
        let restored =
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
                .expect("original documentation attachment facts");
        assert_eq!(&restored, document.facts());
        assert_eq!(
            restored.symbols()[0].item_range,
            document.symbols()[0].item_range
        );
    }
}

#[test]
fn rust_macro_multiline_lifetimes_keep_original_direct_children() {
    let source = "cfg_rt! {\n    pub fn spawn<'a, F>(future: F) -> F\n    where\n        F: Future + Send + 'static,\n        F::Output: Send + 'static,\n    { future }\n}\n";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let captured = parts(document.facts());
    let invocations = captured
        .macro_invocations
        .as_ref()
        .expect("original invocation");
    assert_eq!(invocations.len(), 1);
    let children = invocations[0]
        .2
        .as_ref()
        .expect("complete original lifetime tokens");
    assert!(children.iter().any(|range| &source
        [usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end")]
        == "a"));
    let restored = SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured)
        .expect("original lifetime ranges restore");
    assert_eq!(&restored, document.facts());
}

#[test]
fn rust_macro_lifetime_gaps_refuse_omitted_identifiers_and_character_tokens() {
    let source = "cfg_rt! { fn named<'a>() {} const LETTER: char = 'x'; const ESCAPED: char = '\\n'; const RAW: &str = r#\"held\"#; }";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let captured = parts(document.facts());
    let children = captured.macro_invocations.as_ref().expect("invocation")[0]
        .2
        .as_ref()
        .expect("original direct tokens");
    for expected in ["a", "'x'", "'\\n'", "r#\"held\"#"] {
        let index = children
            .iter()
            .position(|range| {
                &source[usize::try_from(range.start).expect("start")
                    ..usize::try_from(range.end).expect("end")]
                    == expected
            })
            .expect("original named token");
        let mut invalid = captured.clone();
        invalid.macro_invocations.as_mut().expect("invocation")[0]
            .2
            .as_mut()
            .expect("direct tokens")
            .remove(index);
        assert_eq!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), invalid)
                .expect_err("omitted token cannot become anonymous lifetime gap")
                .slug()
                .as_str(),
            "rift.syntax.facts_range_invalid",
            "{expected}"
        );
    }
}

#[test]
fn rust_macro_lifetime_gaps_refuse_malformed_and_truncated_original_groups() {
    for source in [
        "cfg_rt! { ' }",
        "cfg_rt! { '\\ }",
        "cfg_rt! { 'unterminated",
    ] {
        let document = parsed(ShippedLanguage::Rust, source);
        assert!(
            document
                .facts()
                .macro_invocations()
                .is_none_or(|rows| rows.iter().all(|(_, _, children)| children.is_none()))
        );
        let mut captured = parts(document.facts());
        captured.macro_invocations = Some(vec![(
            rift_syntax::ByteRange {
                start: 0,
                end: u64::try_from(source.len()).expect("source length"),
            },
            Vec::new(),
            Some(Vec::new()),
        )]);
        assert_eq!(
            SyntaxFacts::from_parts(source, SyntaxLimits::default(), captured)
                .expect_err("malformed original group cannot claim complete direct children")
                .slug()
                .as_str(),
            "rift.syntax.facts_range_invalid"
        );
    }
}

#[test]
fn literal_macro_functions_keep_original_header_body_and_restore() {
    let source = "cfg_rt! { #[track_caller] pub fn spawn<F>(future: F) -> JoinHandle<F::Output>\nwhere F: Future + Send + 'static, F::Output: Send + 'static, { future } fn private() {} }";
    let document = parsed(ShippedLanguage::Rust, source);
    assert!(!document.has_errors());
    let spawn = document
        .facts()
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "spawn")
        .expect("literal original function");
    assert_eq!(spawn.syntax_element_kind, None);
    assert_eq!(spawn.name_range, None);
    assert_eq!(spawn.kind, "function");
    assert_eq!(spawn.visibility.as_deref(), Some("pub"));
    assert!(
        spawn
            .kinds
            .contains(&rift_protocol::read::SymbolKind::Callable)
    );
    assert!(
        spawn
            .kinds
            .contains(&rift_protocol::read::SymbolKind::Public)
    );
    let body = spawn.body_range.expect("original body");
    let start = usize::try_from(spawn.item_range.start).expect("start");
    let body_start = usize::try_from(body.start).expect("body start");
    let end = usize::try_from(body.end).expect("end");
    assert_eq!(&source[body_start..end], "{ future }");
    assert_eq!(
        spawn.signatures[0].display,
        source[start..body_start].trim_end()
    );
    assert_eq!(spawn.range.end, body.end);
    assert_eq!(document.facts().symbols().len(), 2);
    let restored =
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
            .expect("original function facts restore");
    assert_eq!(&restored, document.facts());
}

#[test]
fn literal_macro_functions_refuse_incomplete_or_nonliteral_headers() {
    for source in [
        "wrapper! { fn broken(a:) {} }",
        "wrapper! { fn broken<T,,>() {} }",
        "wrapper! { fn broken() arbitrary {} }",
        "wrapper! { fn broken() -> {} }",
        "wrapper! { fn broken() where F: {} }",
        "wrapper! { fn broken() -> type_macro!() {} }",
        "wrapper! { fn broken() {",
        "wrapper! { fn broken(a: F {} }",
    ] {
        let document = parsed(ShippedLanguage::Rust, source);
        assert!(
            !document
                .facts()
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "broken"),
            "{source}"
        );
    }
}

#[test]
fn literal_macro_functions_exclude_nested_body_functions_and_keep_duplicate_names() {
    let source = "fn same() {} opaque! { fn same() { fn hidden() {} } fn same() {} } fn plain() {}";
    let document = parsed(ShippedLanguage::Rust, source);
    let names = document
        .facts()
        .symbols()
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"plain"));
    let ordinals = document
        .facts()
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "same")
        .map(|symbol| symbol.declaration_ordinal)
        .collect::<Vec<_>>();
    assert_eq!(ordinals, [0, 1, 2]);
    let duplicate_ranges = document
        .facts()
        .symbols()
        .iter()
        .filter(|symbol| symbol.name == "same")
        .map(|symbol| symbol.item_range.start)
        .collect::<Vec<_>>();
    assert!(duplicate_ranges.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!names.iter().any(|name| name.contains("hidden")));
    let restored =
        SyntaxFacts::from_parts(source, SyntaxLimits::default(), parts(document.facts()))
            .expect("normalization and original duplicate ranges restore");
    assert_eq!(&restored, document.facts());
}

#[test]
fn literal_macro_functions_keep_name_bounds_and_reject_corrupt_body_ranges() {
    let retained_name = "x".repeat(5000);
    let at_bound = "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX);
    let name = "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1);
    let source = format!(
        "opaque! {{ fn {retained_name}() {{}} fn {at_bound}() {{}} fn {name}() {{}} fn kept() {{}} }}"
    );
    let document = parsed(ShippedLanguage::Rust, &source);
    for retained in [&retained_name, &at_bound] {
        let symbol = document
            .facts()
            .symbols()
            .iter()
            .find(|symbol| &symbol.name == retained)
            .expect("provider-bound name remains an original syntax fact");
        let item_start = usize::try_from(symbol.item_range.start).expect("item start");
        let item_end = usize::try_from(symbol.item_range.end).expect("item end");
        let body = symbol.body_range.expect("original body range");
        let body_start = usize::try_from(body.start).expect("body start");
        let body_end = usize::try_from(body.end).expect("body end");
        assert_eq!(
            &source[item_start..item_end],
            format!("fn {retained}() {{}}")
        );
        assert_eq!(&source[body_start..body_end], "{}");
        assert_eq!(symbol.signatures[0].display, format!("fn {retained}()"));
    }
    let restored =
        SyntaxFacts::from_parts(&source, SyntaxLimits::default(), parts(document.facts()))
            .expect("provider-bound original facts restore");
    assert_eq!(&restored, document.facts());
    assert!(
        document
            .facts()
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "kept")
    );
    assert!(
        document
            .facts()
            .left_out_symbols()
            .iter()
            .any(|symbol| symbol.name == name)
    );
    let mut captured = parts(document.facts());
    let kept = captured
        .symbols
        .iter_mut()
        .find(|symbol| symbol.name == "kept")
        .expect("kept function");
    kept.body_range = Some(rift_syntax::ByteRange { start: 0, end: 1 });
    assert!(SyntaxFacts::from_parts(&source, SyntaxLimits::default(), captured).is_err());
}

#[test]
fn literal_macro_functions_obey_original_source_and_syntax_element_bounds() {
    let source = "opaque! { fn kept<F>(future: F) -> F { future } }";
    let language = ShippedLanguage::Rust.language();
    let provider = rift_syntax::registry::provider_for_language(&language).expect("Rust provider");
    let path = ProjectPath::new("source.rs").expect("source path");
    for limits in [
        SyntaxLimits::new(source.len() - 1, 100, 100).expect("byte bound"),
        SyntaxLimits::new(source.len(), 2, 100).expect("syntax element bound"),
    ] {
        assert!(
            provider
                .analyze(
                    SyntaxSource {
                        path: &path,
                        text: source
                    },
                    limits
                )
                .is_err()
        );
    }
}

fn literal_function_header_tokens(source: &str) -> Vec<&str> {
    let mut rows = Vec::new();
    assert!(rift_syntax::tokens(
        source,
        rift_syntax::ByteRange {
            start: 0,
            end: u64::try_from(source.len()).expect("original header length"),
        },
        SyntaxLimits::default(),
        |range| {
            let start = usize::try_from(range.start).expect("token start");
            let end = usize::try_from(range.end).expect("token end");
            rows.push(&source[start..end]);
            true
        },
    ));
    rows
}

#[test]
fn literal_function_headers_keep_original_lifetime_arguments_and_where_bounds() {
    for source in [
        "fn spawn<F>(future: F) -> JoinHandle<F::Output> where F: Future + Send + 'static, F::Output: Send + 'static,",
        "fn spawn_inner<T>(future: T, meta: SpawnMeta<'_>) -> JoinHandle<T::Output> where T: Future + Send + 'static, T::Output: Send + 'static,",
    ] {
        let tokens = literal_function_header_tokens(source);
        assert!(SyntaxDocument::function_header(
            &tokens,
            (tokens.len(), source.len())
        ));
        assert!(tokens.iter().all(|token| source.contains(token)));
        assert_eq!(tokens[0], "fn");
    }
}

#[test]
fn literal_function_headers_refuse_bounds_malformed_and_truncated_tokens() {
    let source = "fn spawn_inner<T>(future: T, meta: SpawnMeta<'_>) -> JoinHandle<T::Output> where T: Future + Send + 'static,";
    let tokens = literal_function_header_tokens(source);
    assert!(!SyntaxDocument::function_header(
        &tokens,
        (tokens.len() - 1, source.len())
    ));
    assert!(!SyntaxDocument::function_header(&tokens, (tokens.len(), 0)));
    for source in [
        "fn spawn_inner<T>(future: T, meta: SpawnMeta<>)",
        "fn spawn_inner<T>(future: T) ->",
        "fn spawn_inner<T>(future: T) where T:",
        "fn spawn_inner<T>(future: T) where T: Future +",
        "fn spawn_inner<T>(future: T) extra",
        "fn spawn_inner<T>(future: T, meta: SpawnMeta<'_>, extra)",
    ] {
        let tokens = literal_function_header_tokens(source);
        assert!(
            !SyntaxDocument::function_header(&tokens, (tokens.len(), source.len())),
            "{source}"
        );
    }
}

const VARIANCE_SOURCE: &str = "export interface Base<T> { base: T }
export interface Mid<out T = unknown, in U = string> extends Base<T> { value: T; run(input: U): T }
export class Box<in out T> { item!: T }
";

#[test]
fn checked_variance_state_restores_equal_to_a_fresh_parse() {
    let limits = SyntaxLimits::default();
    for shipped in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
        let fresh = parsed(shipped, VARIANCE_SOURCE);
        assert!(fresh.has_errors());
        assert!(fresh.facts().declaration_syntax_complete());

        let restored = SyntaxFacts::from_parts(VARIANCE_SOURCE, limits, parts(fresh.facts()))
            .expect("checked variance facts");
        assert!(restored.has_errors());
        assert!(restored.declaration_syntax_complete());
        assert_eq!(&restored, fresh.facts());
    }

    let fresh = parsed(ShippedLanguage::TypeScript, VARIANCE_SOURCE);
    let mut without_errors = parts(fresh.facts());
    without_errors.has_errors = false;
    let mut other_language = parts(parsed(ShippedLanguage::JavaScript, "let = ;").facts());
    assert!(other_language.has_errors);
    other_language.variance_errors_only = true;
    for (source, invalid) in [
        (VARIANCE_SOURCE, without_errors),
        ("let = ;", other_language),
    ] {
        assert_eq!(
            SyntaxFacts::from_parts(source, limits, invalid).map_err(|error| error.slug()),
            Err(errors::syntax::facts_structure_invalid::SLUG)
        );
    }

    let source = "export interface Bad<out T> extends { item: T }";
    let fresh = parsed(ShippedLanguage::TypeScript, source);
    let restored = SyntaxFacts::from_parts(source, limits, parts(fresh.facts()))
        .expect("facts beside other parser errors");
    assert!(restored.has_errors());
    assert!(!restored.declaration_syntax_complete());
    assert_eq!(&restored, fresh.facts());
}
