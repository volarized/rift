//! Physical Rust let bindings and reads kept in their lexical scopes.

use rift_core::ProjectPath;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::index::{PackageAnalysisWarning, PackagePublication, PackageSymbol};
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::ShippedLanguage;

use super::fixture;
use crate::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource};

const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";

fn analyze(source: &str) -> PackagePublication {
    analyze_with_limits(source, ExactPackageLimits::new(2, 1 << 20))
}

fn analyze_with_limits(source: &str, limits: ExactPackageLimits) -> PackagePublication {
    let package = fixture::identity();
    let owner = package.owner().expect("package owner");
    let language = fixture::language(ShippedLanguage::Rust);
    let origin = fixture::origin(&package);
    let root = ProjectPath::new("src/lib.rs").expect("library path");
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let files = [PackageSource::new(&root, source)];
    let metadata = [PackageSource::new(
        &manifest,
        "[package]\nname='beacon'\nversion='1.0.0'\n",
    )];
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
        .expect("bounded package input")
        .with_framework_context(&metadata, &[])
        .expect("captured Cargo manifest");
    PackageAnalyzer::analyze(input, 1)
        .expect("admitted Rust package")
        .into_parts()
        .0
}

fn position(source: &str, text: &str) -> u64 {
    u64::try_from(source.find(text).expect("source token")).expect("source offset")
}

fn declaration_at<'publication>(
    publication: &'publication PackagePublication,
    source: &str,
    text: &str,
) -> &'publication PackageSymbol {
    let start = position(source, text);
    publication
        .declarations
        .iter()
        .find(|declaration| declaration.range.start == start)
        .expect("physical declaration")
}

#[test]
fn package_rust_let_reads_follow_their_physical_shadowing_scope() {
    const BODY: &str = "pub fn choose(parameter: u16) -> u16 {\n    let local = parameter;\n    let nested = { let local = 7u16; local };\n    local + nested\n}\n";
    let original = analyze(BODY);
    let outer = declaration_at(&original, BODY, "local = parameter;");
    let inner = declaration_at(&original, BODY, "local = 7u16;");
    let nested = declaration_at(&original, BODY, "nested =");
    assert_ne!(outer.symbol, inner.symbol);
    assert!(!outer.public && !inner.public && !nested.public);
    for (written, offset, target) in [
        ("local };", 0, inner),
        ("local + nested", 0, outer),
        ("local + nested", 8, nested),
    ] {
        let start = position(BODY, written) + offset;
        let row = original
            .edges
            .iter()
            .find(|row| {
                row.kinds == [EdgeKind::Reads]
                    && row
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.range.start == start)
            })
            .expect("written local read");
        assert_eq!(row.to, target.symbol);
        assert!(row.from.as_str().ends_with("/choose"));
    }
    let shifted = format!("// Added without changing a binding.\n{BODY}");
    let shifted_publication = analyze(&shifted);
    for (written, before) in [("local = parameter;", outer), ("local = 7u16;", inner)] {
        assert_eq!(
            declaration_at(&shifted_publication, &shifted, written).symbol,
            before.symbol
        );
    }
}

#[test]
fn package_rust_let_scope_keeps_closures_and_imported_value_paths() {
    let source = "pub mod items { pub const BASE: u16 = 3; }\nuse items::BASE as imported;\npub fn choose() -> u16 {\n    let local = || 1u16;\n    let before = local();\n    let local = before;\n    let selected = crate::items::BASE;\n    let copied = imported;\n    local + selected + copied\n}\n";
    let result = analyze(source);
    let closure = declaration_at(&result, source, "let local = ||");
    let local = declaration_at(&result, source, "local = before;");
    assert_ne!(closure.symbol, local.symbol);
    assert!(result.edges.iter().any(|row| {
        row.kinds == [EdgeKind::Calls]
            && row.to == closure.symbol
            && row
                .occurrence
                .as_ref()
                .is_some_and(|site| site.range.start == position(source, "local()"))
    }));
    for written in ["BASE;\n    let copied", "imported;\n    local"] {
        assert!(result.edges.iter().any(|row| {
            row.kinds == [EdgeKind::Reads]
                && row.from.as_str().ends_with("/choose")
                && row.to.as_str().ends_with("/items/BASE")
                && row
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range.start == position(source, written))
        }));
    }
}

#[test]
fn package_rust_let_constant_pattern_has_no_local_declaration() {
    for included in [
        "#[allow(non_upper_case_globals)] const local: () = ();",
        "pub mod values { pub const VALUE: () = (); }\nuse values::VALUE as local;",
    ] {
        let source = format!("{included}\npub fn choose() {{ let local = (); local }}\n");
        let result = analyze(&source);
        let pattern = position(&source, "local = ();");
        assert!(
            result
                .declarations
                .iter()
                .all(|declaration| declaration.range.start != pattern)
        );
    }
}

#[test]
fn package_rust_let_does_not_reserve_identity_for_an_inactive_earlier_binding() {
    let source = "pub fn choose() -> u8 { #[cfg(any())] let local = 3; let local = 7; local }";
    let result = analyze(source);
    let inactive = position(source, "local = 3");
    assert!(
        result
            .declarations
            .iter()
            .all(|row| row.range.start != inactive)
    );
    let active = declaration_at(&result, source, "local = 7");
    assert!(active.symbol.as_str().ends_with("/local%230"));
    assert!(result.edges.iter().any(|row| {
        row.kinds == [EdgeKind::Reads]
            && row.to == active.symbol
            && row
                .occurrence
                .as_ref()
                .is_some_and(|site| site.range.start == position(source, "local }"))
    }));
}

#[test]
fn package_rust_let_does_not_displace_older_declarations_or_edges() {
    let source = format!(
        "pub fn leaf() {{}}\npub fn choose() -> u16 {{ {} let local = 3; local }}\n",
        "leaf(); ".repeat(128),
    );
    let limits = || ExactPackageLimits::new(2, 1 << 20);
    let full = analyze(&source);
    let local = declaration_at(&full, &source, "local = 3;").symbol.clone();
    let declarations = full
        .declarations
        .iter()
        .filter(|row| row.symbol != local && row.symbol.as_str() != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    let edges = full
        .edges
        .iter()
        .filter(|row| row.to != local && row.from != local && row.from.as_str() != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(edges.len(), 128);
    let no_declaration_room = analyze_with_limits(
        &source,
        limits().with_declarations(2).expect("primary bound"),
    );
    assert_eq!(no_declaration_room.declarations, declarations);
    assert_eq!(no_declaration_room.edges, edges);
    assert!(no_declaration_room.warnings.iter().any(|warning| matches!(
        warning,
        PackageAnalysisWarning::PublicationTruncated { collection, .. }
            if collection == "local_declarations"
    )));
    let no_publication_room = analyze_with_limits(
        &source,
        limits()
            .with_publication(PackageConfiguration {
                symbols: 2,
                ..PackageConfiguration::default()
            })
            .expect("primary publication bound"),
    );
    assert_eq!(no_publication_room.declarations, declarations);
    assert_eq!(no_publication_room.edges, edges);
    let no_edge_room = analyze_with_limits(
        &source,
        limits().with_edges(edges.len()).expect("primary edge cap"),
    );
    assert_eq!(no_edge_room.declarations, full.declarations);
    assert_eq!(no_edge_room.edges, edges);
    assert!(no_edge_room.warnings.iter().any(|warning| matches!(
        warning,
        PackageAnalysisWarning::PublicationTruncated { collection, .. }
            if collection == "edges"
    )));
}

#[test]
fn package_rust_let_keeps_exact_primary_retained_source_budget() {
    let source = "pub fn leaf() {}\npub fn choose() -> u16 { leaf(); let local = 3; local }\n";
    let record = 64;
    let limits = |total| {
        ExactPackageLimits::new(2, 1 << 20)
            .with_retained_source_bytes(record, total)
            .expect("retained source bound")
    };
    let primary = analyze_with_limits(
        source,
        limits(u64::MAX)
            .with_declarations(2)
            .expect("two functions"),
    );
    let bytes = primary
        .units
        .iter()
        .map(|unit| unit.source.as_str())
        .chain(primary.declarations.iter().map(|row| row.source.as_str()))
        .chain(primary.documents.iter().flat_map(|doc| {
            [
                doc.file_content.as_deref(),
                doc.declaration_source.as_deref(),
            ]
            .into_iter()
            .flatten()
        }))
        .map(|text| u64::try_from(text.len()).expect("source length"))
        .sum();
    let exact = analyze_with_limits(source, limits(bytes));
    assert_eq!(exact.declarations, primary.declarations);
    assert_eq!(exact.edges, primary.edges);
    assert_eq!(exact.units, primary.units);
}

#[test]
fn package_rust_parameter_names_keep_their_callable_and_physical_read() {
    let source = "pub fn choose(parameter: u16) -> u16 {\n    let local = parameter;\n    let nested = { let local = 7u16; local };\n    local + nested\n}\npub fn alternate(parameter: u8) -> u8 { parameter }\n";
    let result = analyze(source);
    let first = declaration_at(&result, source, "parameter: u16");
    let second = declaration_at(&result, source, "parameter: u8");
    assert_ne!(first.symbol, second.symbol);
    assert!(!first.public && !second.public);
    for (parameter, function, read) in [
        (first, "/choose", "parameter;"),
        (second, "/alternate", "parameter }"),
    ] {
        assert!(result.objects.iter().any(|symbol| {
            symbol.id.as_ref() == Some(&parameter.symbol)
                && symbol.kinds.contains(&SymbolKind::Parameter)
        }));
        let site = position(source, read);
        assert!(result.edges.iter().any(|row| {
            row.kinds == [EdgeKind::Reads]
                && row.from.as_str().ends_with(function)
                && row.to == parameter.symbol
                && row.occurrence.as_ref().is_some_and(|site_span| {
                    site_span.range.start == site && site_span.range.end == site + 9
                })
        }));
    }
}

#[test]
fn package_rust_parameter_shadowing_keeps_the_initializer_and_call_owner() {
    let source = "pub fn leaf() {}\npub fn choose(parameter: u16) -> u16 {\n    let previous = parameter;\n    let parameter = parameter + 1;\n    leaf();\n    previous + parameter\n}\n";
    let result = analyze(source);
    let parameter = declaration_at(&result, source, "parameter: u16");
    let shadow = declaration_at(&result, source, "parameter = parameter");
    assert_ne!(parameter.symbol, shadow.symbol);
    assert!(
        parameter
            .symbol
            .as_str()
            .ends_with("/parameter%23parameter0")
    );
    assert!(shadow.symbol.as_str().ends_with("/parameter%230"));
    for (written, target) in [
        ("parameter;", parameter),
        ("parameter + 1", parameter),
        ("parameter\n", shadow),
    ] {
        let site = position(source, written);
        assert!(result.edges.iter().any(|row| {
            row.kinds == [EdgeKind::Reads]
                && row.from.as_str().ends_with("/choose")
                && row.to == target.symbol
                && row
                    .occurrence
                    .as_ref()
                    .is_some_and(|span| span.range.start == site)
        }));
    }
    assert!(result.edges.iter().any(|row| {
        row.kinds == [EdgeKind::Calls]
            && row.from.as_str().ends_with("/choose")
            && row.to.as_str().ends_with("/leaf")
    }));
}

#[test]
fn package_rust_parameter_leaves_the_existing_let_slot_available() {
    let source = "pub fn choose(parameter: u16) -> u16 { let local = parameter; local }";
    let limits = ExactPackageLimits::new(2, 1 << 20)
        .with_declarations(2)
        .expect("room for the function and its earlier local");
    let result = analyze_with_limits(source, limits);
    let local = declaration_at(&result, source, "local = parameter;");
    let parameter = position(source, "parameter: u16");
    assert!(
        result
            .declarations
            .iter()
            .all(|row| row.range.start != parameter)
    );
    assert!(
        result
            .edges
            .iter()
            .any(|row| { row.kinds == [EdgeKind::Reads] && row.to == local.symbol })
    );
}

#[test]
fn package_rust_receiver_parameters_keep_their_method_and_physical_reads() {
    let source = r"
mod support { pub const ZERO: u8 = 0; }
use support::*;
pub trait Observed { fn observe(&self); }
pub struct Counter { value: u8 }
impl Counter {
    pub fn current(&self) -> u8 { self.value }
    pub fn owned(self) -> u8 { self.current() + ZERO }
    pub fn boxed(self: Box<Self>) -> u8 { self.current() + self::support::ZERO }
}
";
    let result = analyze(source);
    for (written, method, expression) in [
        ("self);", "/observe", None),
        ("self) -> u8 { self.value", "/current", Some("self.value")),
        (
            "self) -> u8 { self.current",
            "/owned",
            Some("self.current() + ZERO"),
        ),
        ("self: Box<Self>", "/boxed", Some("self.current() + self::")),
    ] {
        let receiver = declaration_at(&result, source, written);
        assert!(!receiver.public);
        assert_eq!(receiver.range.end, receiver.range.start + 4);
        assert!(result.objects.iter().any(|symbol| {
            symbol.id.as_ref() == Some(&receiver.symbol)
                && symbol.kinds.contains(&SymbolKind::Parameter)
        }));
        assert!(result.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Contains]
                && edge.from.as_str().ends_with(method)
                && edge.to == receiver.symbol
        }));
        let reads = result
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to == receiver.symbol)
            .collect::<Vec<_>>();
        if let Some(expression) = expression {
            let [read] = reads.as_slice() else {
                panic!("one receiver Read");
            };
            assert!(read.from.as_str().ends_with(method));
            let span = read.occurrence.as_ref().expect("receiver Read range");
            let start = position(source, expression);
            assert_eq!((span.range.start, span.range.end), (start, start + 4));
        } else {
            assert!(reads.is_empty());
        }
    }
    assert!(result.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/current")
            && edge.to.as_str().ends_with("/value")
    }));
    for method in ["/owned", "/boxed"] {
        assert!(result.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from.as_str().ends_with(method)
                && edge.to.as_str().ends_with("/current")
        }));
    }
}

#[test]
fn package_rust_receiver_leaves_established_declarations_and_edges_at_the_caps() {
    let source = &format!(
        "pub fn leaf() {{}}
pub struct Counter {{ value: u8 }}
impl Counter {{
    pub fn current(&self, parameter: u8) -> u8 {{ {} let local = parameter; self.value + local }}
}}
",
        "leaf(); ".repeat(128),
    );
    let full = analyze(source);
    let receiver = &declaration_at(&full, source, "self, parameter").symbol;
    let declarations = full
        .declarations
        .iter()
        .filter(|row| &row.symbol != receiver && row.symbol.as_str() != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    let edges = full
        .edges
        .iter()
        .filter(|edge| &edge.to != receiver && &edge.from != receiver && edge.from.as_str() != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        full.edges
            .iter()
            .filter(|edge| &edge.to == receiver)
            .count(),
        2
    );
    let no_declaration_room = analyze_with_limits(
        source,
        ExactPackageLimits::new(2, 1 << 20)
            .with_declarations(u32::try_from(declarations.len()).expect("declaration count"))
            .expect("room for established declarations"),
    );
    assert_eq!(no_declaration_room.declarations, declarations);
    assert_eq!(no_declaration_room.edges, edges);
    let no_edge_room = analyze_with_limits(
        source,
        ExactPackageLimits::new(2, 1 << 20)
            .with_edges(edges.len())
            .expect("room for established edges"),
    );
    assert_eq!(no_edge_room.declarations, full.declarations);
    assert_eq!(no_edge_room.edges, edges);
}

#[test]
fn package_rust_generic_parameters_keep_their_physical_item_scopes() {
    let source = "pub struct Pair<T>(T);
impl<T> Pair<T> {
    pub fn pick<'a, U, const N: usize>(&self, values: &'a [U; N]) -> Option<&'a U> { values.first() }
}
pub fn keep<U>(value: U) -> U { value }
";
    let result = analyze(source);
    let generics = [
        ("T>(T)", "/Pair", SymbolKind::TypeParameter, 1),
        ("T> Pair", "/Pair", SymbolKind::TypeParameter, 1),
        ("'a, U", "/Pair/pick", SymbolKind::TypeParameter, 2),
        ("U, const N", "/Pair/pick", SymbolKind::TypeParameter, 1),
        ("N: usize", "/Pair/pick", SymbolKind::Parameter, 1),
        ("U>(value", "/keep", SymbolKind::TypeParameter, 1),
    ];
    let mut identities = std::collections::BTreeSet::new();
    for (written, scope, kind, length) in generics {
        let parameter = declaration_at(&result, source, written);
        assert!(!parameter.public);
        assert_eq!(parameter.range.end, parameter.range.start + length);
        assert!(parameter.symbol.as_str().contains(scope));
        assert!(identities.insert(&parameter.symbol));
        assert!(result.objects.iter().any(|object| {
            object.id.as_ref() == Some(&parameter.symbol) && object.kinds.contains(&kind)
        }));
    }
    let generic_uses = [
        ("T>(T)", "T);", 0, EdgeKind::HasType, 1),
        ("T> Pair", "T> {", 0, EdgeKind::HasType, 1),
        ("'a, U", "'a [U", 0, EdgeKind::HasType, 2),
        ("'a, U", "'a U>", 0, EdgeKind::HasType, 2),
        ("U, const N", "[U; N]", 1, EdgeKind::HasType, 1),
        ("N: usize", "N])", 0, EdgeKind::Reads, 1),
        ("U>(value", "U) -> U", 0, EdgeKind::HasType, 1),
    ];
    for (declaration, written, offset, kind, len) in generic_uses {
        let parameter = declaration_at(&result, source, declaration);
        let start = position(source, written) + offset;
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [kind]
                    && edge.to == parameter.symbol
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        (site.range.start, site.range.end) == (start, start + len)
                    })
            }),
            "{written} resolves to {declaration}"
        );
    }
}

#[test]
fn package_rust_shorthand_struct_patterns_keep_binding_and_field_separate() {
    let source = "pub struct Panel { pub item: u8 }
pub fn choose(panel: Panel) -> u8 {
    let Panel { item } = panel;
    item
}
pub fn branch(panel: Panel) -> u8 {
    match panel {
        Panel { item } if item > 3 => item,
        Panel { item } => item + 1,
    }
}
pub fn parameter(Panel { item }: Panel) -> u8 {
    item
}
";
    let result = analyze(source);
    let field = result
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/Panel/item"))
        .expect("separate field declaration");
    assert!(field.public);
    for (name, at, range, reads) in [
        ("choose", "item } = panel", "item\n}", 1),
        ("branch", "item } if", "item > 3", 2),
        ("branch", "item } =>", "item + 1", 1),
        ("parameter", "item }: Panel", "item\n}\n", 1),
    ] {
        let declaration = declaration_at(&result, source, at);
        assert_eq!(declaration.source, "item");
        assert!(!declaration.public);
        assert_ne!(declaration.symbol, field.symbol);
        assert!(
            declaration
                .symbol
                .as_str()
                .contains(&format!("/{name}/item%23shorthand"))
        );
        let occurrences = result
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads])
            .filter_map(|edge| edge.occurrence.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(occurrences.len(), reads, "{at}");
        let start = if name == "parameter" {
            u64::try_from(source.rfind(range).expect("parameter read")).expect("byte offset")
        } else {
            position(source, range)
        };
        assert!(
            occurrences
                .iter()
                .any(|site| site.range.start == start && site.range.end == start + 4)
        );
    }
    let established = result
        .declarations
        .iter()
        .filter(|row| !row.symbol.as_str().contains("%23shorthand"))
        .cloned()
        .collect::<Vec<_>>();
    let bounded = analyze_with_limits(
        source,
        ExactPackageLimits::new(2, 1 << 20)
            .with_declarations(u32::try_from(established.len()).expect("established rows"))
            .expect("room for previous declarations"),
    );
    assert_eq!(bounded.declarations, established);
    assert!(
        result
            .edges
            .iter()
            .filter(|edge| {
                !edge.from.as_str().contains("%23shorthand")
                    && !edge.to.as_str().contains("%23shorthand")
            })
            .all(|edge| bounded.edges.contains(edge))
    );
}

#[test]
fn exact_rust_definition_shorthand_is_admitted_after_existing_bindings() {
    use rift_core::{FileDigest, SourceRange};
    use rift_protocol::producer::{FactProducer, ProducerName, ProducerRevision};

    use crate::{RustLspDeclaration, RustLspDeclarationFile};

    let source = "pub struct Panel { pub item: u8 }
macro_rules! notice { () => { () } }
pub fn choose(panel: Panel) -> u8 {
    notice!();
    let Panel { item } = panel;
    item
}
";
    let base = analyze(source);
    let token = position(source, "item } = panel");
    let field = position(source, "item: u8");
    let span = |start| SourceRange::new(start, start + 4).expect("physical token");
    let producer = FactProducer {
        name: ProducerName::parse("rust-analyzer").expect("producer"),
        revision: ProducerRevision::parse("0.0.1").expect("producer revision"),
    };
    let path = ProjectPath::new("src/lib.rs").expect("opened Rust source");
    let declarations = [
        RustLspDeclaration {
            range: span(field),
            parameter: false,
        },
        RustLspDeclaration {
            range: span(token),
            parameter: false,
        },
    ];
    let observed = [RustLspDeclarationFile {
        path: &path,
        digest: FileDigest::of(source.as_bytes()),
        declarations: &declarations,
    }];
    assert!(base.declarations.iter().all(|row| row.range.start != token));
    let mut admitted = base.clone();
    let limits = ExactPackageLimits::new(2, 1 << 20);
    let result = PackageAnalyzer::admit_rust_lsp_shorthand_declarations(
        &mut admitted,
        &observed,
        &producer,
        limits,
    )
    .expect("verified Definition observation");
    assert_eq!(result.admitted, 1);
    let added = admitted
        .declarations
        .iter()
        .find(|row| row.range.start == token)
        .expect("authored shorthand definition");
    assert_eq!(added.range.end, token + 4);
    assert!(
        added
            .symbol
            .as_str()
            .ends_with("/choose/item%23lsp_shorthand0")
    );
    assert_eq!(
        added.syntax_element_kind.as_deref(),
        Some("shorthand_field_identifier")
    );
    assert!(
        base.declarations
            .iter()
            .all(|row| admitted.declarations.contains(row))
    );
    assert_eq!(admitted.edges, base.edges);

    let clipped = limits
        .with_declarations(u32::try_from(base.declarations.len()).expect("cap"))
        .expect("room for incumbent declarations");
    let mut held = base.clone();
    let result = PackageAnalyzer::admit_rust_lsp_shorthand_declarations(
        &mut held, &observed, &producer, clipped,
    )
    .expect("checked capped observation");
    assert_eq!(result.dropped_at_limit, 1);
    assert_eq!(held, base);
}

#[test]
fn package_rust_shorthand_refuses_an_in_scope_constant() {
    let source = "const item: u8 = 0;
pub struct Panel { pub item: u8 }
pub fn test(panel: Panel) -> bool {
    match panel {
        Panel { item } => true,
        _ => false,
    }
}
";
    let result = analyze(source);
    assert!(
        result
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/Panel/item"))
    );
    assert!(result.declarations.iter().all(|row| {
        row.range.start != position(source, "item }")
            && !row.symbol.as_str().contains("%23shorthand")
    }));
}

#[test]
fn package_rust_explicit_lets_keep_declarations_when_macro_output_is_opaque() {
    let source = "mod values { #[allow(non_upper_case_globals)] pub const constant: () = (); }
use values::*;
macro_rules! shadow { ($name:ident) => { let $name = 3u8; } }
pub fn choose() -> u8 {
    let constant = ();
    #[cfg(any())] let mut local = 0;
    let mut local = 1;
    {
        let mut local = 2;
        local += 1;
    }
    shadow!(local);
    local
}
";
    let full = analyze(source);
    let first = declaration_at(&full, source, "local = 1");
    let second = declaration_at(&full, source, "local = 2");
    assert_ne!(first.symbol, second.symbol);
    assert!(first.symbol.as_str().ends_with("/local%23explicit0"));
    assert!(second.symbol.as_str().ends_with("/local%23explicit1"));
    assert!(!first.public && !second.public);
    for excluded in ["constant = ();", "local = 0", "local);", "local\n}"] {
        assert!(
            full.declarations
                .iter()
                .all(|row| row.range.start != position(source, excluded))
        );
    }
    assert!(full.edges.iter().all(|edge| edge.kinds != [EdgeKind::Reads]
        || (edge.to != first.symbol && edge.to != second.symbol)));

    let previous = full
        .declarations
        .iter()
        .filter(|row| row.symbol != first.symbol && row.symbol != second.symbol)
        .cloned()
        .collect::<Vec<_>>();
    let bounded = analyze_with_limits(
        source,
        ExactPackageLimits::new(2, 1 << 20)
            .with_declarations(u32::try_from(previous.len()).expect("declaration count"))
            .expect("room for established declarations"),
    );
    assert_eq!(bounded.declarations, previous);
    assert!(
        full.edges
            .iter()
            .filter(|row| row.to != first.symbol && row.to != second.symbol)
            .all(|row| bounded.edges.contains(row))
    );
}

#[test]
fn package_rust_lsp_declarations_add_only_anchored_bindings_under_their_callable() {
    use crate::{RustLspDeclaration, RustLspDeclarationFile};
    use rift_core::{FileDigest, SourceRange};
    use rift_protocol::producer::{FactProducer, ProducerName, ProducerRevision};

    let source = "macro_rules! bind { ($name:ident) => { let $name = 1u8; } }
pub fn choose(pair: (u8, u8)) -> u8 {
    let (left, right) = pair;
    bind!(expanded);
    left + right + expanded
}
";
    let existing = analyze(source);
    let file = ProjectPath::new("src/lib.rs").expect("Rust source path");
    let observations = [
        ("pair:", 4, true),
        ("left,", 4, false),
        ("right)", 5, false),
        ("expanded);", 8, false),
        ("left +", 4, false),
        ("choose(", 6, false),
    ]
    .map(|(token, size, parameter)| {
        let start = position(source, token);
        RustLspDeclaration {
            range: SourceRange::new(start, start + size).expect("ranged token"),
            parameter,
        }
    });
    let producer = FactProducer {
        name: ProducerName::parse("rust-analyzer").expect("producer"),
        revision: ProducerRevision::parse("1.0.0").expect("revision"),
    };
    let limits = ExactPackageLimits::new(2, 1 << 20);
    let admit = |base: &mut PackagePublication, digest, limits| {
        PackageAnalyzer::admit_rust_lsp_declarations(
            base,
            &[RustLspDeclarationFile {
                path: &file,
                digest,
                declarations: &observations,
            }],
            &producer,
            limits,
        )
        .expect("complete owned binding admission")
    };
    let mut augmented = existing.clone();
    let wrong = admit(&mut augmented, FileDigest::of(b"other bytes"), limits);
    assert_eq!(wrong.admitted, 0);
    assert_eq!(augmented, existing);
    let digest = FileDigest::of(source.as_bytes());
    assert_eq!(admit(&mut augmented, digest, limits).admitted, 3);

    for token in &observations[..3] {
        let record = augmented
            .declarations
            .iter()
            .find(|decl| decl.range.start == token.range.start())
            .expect("exact token declaration");
        assert_eq!(record.range.end, token.range.end());
        assert!(!record.public && record.source_complete);
        let object = augmented
            .objects
            .iter()
            .find(|object| object.id.as_ref() == Some(&record.symbol))
            .expect("binding object");
        assert!(
            object
                .container
                .as_ref()
                .is_some_and(|container| container.as_str().ends_with("/choose"))
        );
        assert!(object.document_local);
        assert_eq!(
            object.kinds,
            [if token.parameter {
                SymbolKind::Parameter
            } else {
                SymbolKind::Value
            }]
        );
    }
    assert_eq!(augmented.edges, existing.edges);
    assert!(
        existing
            .declarations
            .iter()
            .all(|row| augmented.declarations.contains(row))
    );
    assert_eq!(admit(&mut augmented, digest, limits).admitted, 0);
    let mut capped = existing.clone();
    let cap = limits
        .with_declarations(u32::try_from(existing.declarations.len()).expect("incumbent count"))
        .expect("incumbent bound");
    assert_eq!(admit(&mut capped, digest, cap).dropped_at_limit, 3);
    assert_eq!(capped, existing);
}

#[test]
fn package_rust_value_paths_read_direct_arguments_without_replacing_calls() {
    let source = "pub mod limits { pub const MAXIMUM: u8 = 2; }
pub fn leaf() -> u8 { 1 }
pub fn consume(number: u8, pending: fn() -> u8) -> u8 { number + pending() }
pub fn run() -> u8 { consume(limits::MAXIMUM, leaf) }
pub fn shadow(leaf: fn() -> u8) -> u8 { consume(0, leaf) }
#[cfg(any())]
pub fn inactive() -> u8 { consume(limits::MAXIMUM, leaf) }
";
    let source = &format!("{source}\npub fn bulk() {{ {} }}", "leaf(); ".repeat(128));
    let full = analyze(source);
    let argument_reads = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/run")
            && (edge.to.as_str().ends_with("/limits/MAXIMUM")
                || edge.to.as_str().ends_with("/leaf"))
    };
    for (written, target, span) in [
        ("MAXIMUM, leaf", "/limits/MAXIMUM", 7),
        ("leaf) }", "/leaf", 4),
    ] {
        assert!(full.edges.iter().any(|edge| {
            let start = position(source, written);
            argument_reads(edge)
                && edge.to.as_str().ends_with(target)
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range.start == start && site.range.end == start + span)
        }));
    }
    assert!(full.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/run")
            && edge.to.as_str().ends_with("/consume")
    }));
    assert!(full.edges.iter().all(|edge| {
        edge.kinds != [EdgeKind::Reads]
            || !(edge.from.as_str().ends_with("/inactive")
                || (edge.from.as_str().ends_with("/shadow")
                    && edge.to.as_str().ends_with("/leaf")
                    && !edge.to.as_str().contains("/shadow/")))
    }));
    let established = full
        .edges
        .iter()
        .filter(|edge| !argument_reads(edge))
        .cloned()
        .collect::<Vec<_>>();
    let bounded = analyze_with_limits(
        source,
        ExactPackageLimits::new(2, 1 << 20)
            .with_edges(established.len())
            .expect("existing edge capacity"),
    );
    assert_eq!(bounded.edges, established);
}

#[test]
fn package_rust_grouped_imports_keep_written_qualifiers_and_terminal_imports() {
    const SOURCE: &str = concat!(
        "pub mod serial { pub mod wire {\n",
        "    pub struct Code; pub struct Marker; pub struct Late; pub struct Base;\n",
        "} }\n",
        "use crate::serial::{wire::Code, wire::Marker};\n",
        "pub use self::serial::{wire::Base as Public};\n",
        "pub mod nested { pub use super::serial::{wire::Late as Local}; }\n",
        "#[cfg(any())]\n",
        "use crate::serial::{wire::Code as Inactive};\n",
        "pub fn run(_: Code, _: Marker) {}\n",
    );
    let full = analyze(SOURCE);
    let mut expected = std::collections::BTreeSet::new();
    for (path, qualifiers) in [
        (
            "crate::serial::{wire::Code, wire::Marker}",
            &[
                ("crate", ""),
                ("serial", "/serial"),
                ("wire::Code", "/serial/wire"),
                ("wire::Marker", "/serial/wire"),
            ][..],
        ),
        (
            "self::serial::{wire::Base as Public}",
            &[
                ("self", ""),
                ("serial", "/serial"),
                ("wire", "/serial/wire"),
            ],
        ),
        (
            "super::serial::{wire::Late as Local}",
            &[
                ("super", ""),
                ("serial", "/serial"),
                ("wire", "/serial/wire"),
            ],
        ),
    ] {
        let start = position(SOURCE, path);
        for (token, suffix) in qualifiers {
            let offset = start + position(path, token);
            let spelling = token.split("::").next().expect("qualifier spelling");
            let stop = offset + u64::try_from(spelling.len()).expect("token bytes");
            assert!(expected.insert((offset, stop, format!("{ROOT}{suffix}"))));
        }
    }

    let qualifier = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.occurrence.as_ref().is_some_and(|site| {
                let end = position(SOURCE, "pub fn run");
                let start = position(SOURCE, "use crate::");
                site.range.start >= start && site.range.end <= end
            })
    };
    let actual = full
        .edges
        .iter()
        .filter(|edge| qualifier(edge))
        .map(|edge| {
            let site = edge.occurrence.as_ref().expect("written qualifier");
            (
                site.range.start,
                site.range.end,
                edge.to.as_str().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), expected.len(), "{actual:?}");
    assert_eq!(
        actual
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    for terminal in ["Code", "Marker", "Base", "Late"] {
        assert!(full.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Imports]
                && edge.to.as_str() == format!("{ROOT}/serial/wire/{terminal}")
        }));
    }
}
