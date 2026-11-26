//! Physical Rust let bindings and reads kept in their lexical scopes.

use rift_core::ProjectPath;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::index::{PackageAnalysisWarning, PackagePublication, PackageSymbol};
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::ShippedLanguage;

use super::fixture;
use crate::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource};

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
        .filter(|row| row.symbol != local)
        .cloned()
        .collect::<Vec<_>>();
    let edges = full
        .edges
        .iter()
        .filter(|row| row.to != local && row.from != local)
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
        .filter(|row| &row.symbol != receiver)
        .cloned()
        .collect::<Vec<_>>();
    let edges = full
        .edges
        .iter()
        .filter(|edge| &edge.to != receiver && &edge.from != receiver)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(full.edges.len(), edges.len() + 2);
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
