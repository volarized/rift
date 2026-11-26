//! Physical Rust let bindings and reads kept in their lexical scopes.

use rift_core::ProjectPath;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::index::{PackageAnalysisWarning, PackagePublication, PackageSymbol};
use rift_protocol::read::EdgeKind;
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
