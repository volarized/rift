#[test]
fn complete_source_inventory_keeps_parser_error_coverage_separate() {
    let source = "# Lone";
    let analysis = super::fixture::package_analysis(
        rift_syntax::ShippedLanguage::Markdown,
        vec![("README.md", source)],
    );
    let syntax = analysis.files()[0].file().syntax();
    assert!(syntax.has_errors());
    assert_eq!(syntax.left_out_declaration_count(), 0);
    assert_eq!(analysis.files()[0].file().source(), source);
    let publication = analysis.publication();
    assert_eq!(publication.units.len(), 1);
    assert!(publication.units[0].source_complete);
    assert_eq!(publication.units[0].source, source);
    let coverage = &publication.coverage[0];
    assert!(coverage.inventory_complete);
    assert!(!coverage.identity_complete);
    assert!(!coverage.applicability_complete);
    assert!(coverage.relationships.is_empty());
}

#[test]
fn complete_source_inventory_keeps_captured_declaration_file_parser_errors_separate() {
    // Original admitted @types/node 26.6.4 buffer.d.ts, retained without editing.
    let source = include_str!("../../tests/fixtures/types-node-buffer.d.ts");
    let analysis = super::fixture::package_analysis(
        rift_syntax::ShippedLanguage::TypeScript,
        vec![("buffer.d.ts", source)],
    );
    let syntax = analysis.files()[0].file().syntax();
    assert!(syntax.has_errors());
    assert_eq!(syntax.left_out_declaration_count(), 0);
    assert_eq!(analysis.files()[0].file().source(), source);
    assert!(
        syntax
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "isUtf8")
    );
    let publication = analysis.publication();
    assert_eq!(publication.units.len(), 1);
    assert!(publication.units[0].source_complete);
    assert_eq!(publication.units[0].source, source);
    let coverage = &publication.coverage[0];
    assert!(coverage.inventory_complete);
    assert!(!coverage.identity_complete);
    assert!(!coverage.applicability_complete);
    assert!(coverage.relationships.is_empty());
}
