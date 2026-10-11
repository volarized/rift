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
// Exact blockquote heading bytes from Prettier 3.8.5 THIRD-PARTY-NOTICES.md.
const REFUSED_HEADING_SOURCE: &str = "> wcwidth.js: a javascript portng of C's wcwidth()\n> ================================================\n>\n";

fn refused_heading_analysis() -> super::PackageAnalysis {
    super::fixture::package_analysis(
        rift_syntax::ShippedLanguage::Markdown,
        vec![("THIRD-PARTY-NOTICES.md", REFUSED_HEADING_SOURCE)],
    )
}

fn retained_heading_records(analysis: &super::PackageAnalysis) -> super::Records {
    super::Records {
        units: analysis.publication().units.clone(),
        ..super::Records::default()
    }
}

#[test]
fn retained_refused_heading_keeps_source_inventory_complete() {
    let analysis = refused_heading_analysis();
    let syntax = analysis.files()[0].file().syntax();
    assert_eq!(syntax.left_out_declaration_count(), 1);
    assert_eq!(syntax.left_out_symbols().len(), 1);
    let refused = &syntax.left_out_symbols()[0];
    assert_eq!(
        refused.name,
        "wcwidth.js: a javascript portng of C's wcwidth()\n>"
    );
    let publication = analysis.publication();
    assert_eq!(publication.units[0].source, REFUSED_HEADING_SOURCE);
    assert!(publication.units[0].source_complete);
    let object = publication
        .objects
        .iter()
        .find(|object| object.name == refused.name)
        .expect("original refused heading retained");
    assert!(object.id.is_none());
    assert!(publication.coverage[0].inventory_complete);
    assert!(!publication.coverage[0].identity_complete);
    assert!(!publication.coverage[0].applicability_complete);
    assert!(publication.coverage[0].relationships.is_empty());
}

#[test]
fn unretained_refused_heading_count_keeps_inventory_incomplete() {
    let mut analysis = refused_heading_analysis();
    let held = &mut analysis.files[0];
    let original = held.file.syntax();
    let parts = rift_syntax::SyntaxFactsParts {
        origin: original.origin(),
        language: original.language().clone(),
        symbols: original.symbols().to_vec(),
        left_out_symbols: original.left_out_symbols().to_vec(),
        export_bindings: original.export_bindings().map(<[_]>::to_vec),
        macro_invocations: None,
        scope: None,
        has_errors: original.has_errors(),
        left_out_declarations: original.left_out_declaration_count() + 1,
        markdown_facts: original.markdown_facts().cloned(),
        source_digest: *original.source_digest().expect("captured digest"),
    };
    let facts = rift_syntax::SyntaxFacts::from_parts(
        held.file.source(),
        rift_syntax::SyntaxLimits::default(),
        parts,
    )
    .expect("known unretained declaration count remains valid");
    held.file = crate::IndexedFile::new_with_shared_syntax(
        held.file.path().clone(),
        std::sync::Arc::new(held.file.source().to_owned()),
        held.file.digest(),
        held.file.executable(),
        std::sync::Arc::new(facts),
    );
    let mut records = retained_heading_records(&analysis);
    let retained_syntax_complete = records.retained_syntax(&analysis.files);
    assert_eq!(records.objects.len(), 1);
    assert!(!retained_syntax_complete);
    assert!(
        !super::publication_coverage(&analysis.files, &records, false, retained_syntax_complete)[0]
            .inventory_complete
    );
}

#[test]
fn refused_heading_object_truncation_keeps_inventory_incomplete() {
    let analysis = refused_heading_analysis();
    let mut records = retained_heading_records(&analysis);
    records.publication.symbols = 0;
    let retained_syntax_complete = records.retained_syntax(&analysis.files);
    assert!(records.symbols_truncated);
    assert!(records.objects.is_empty());
    assert!(!retained_syntax_complete);
    assert!(
        !super::publication_coverage(&analysis.files, &records, false, retained_syntax_complete)[0]
            .inventory_complete
    );
}

#[test]
fn refused_heading_source_truncation_keeps_inventory_incomplete() {
    let analysis = refused_heading_analysis();
    let mut records = retained_heading_records(&analysis);
    records.units[0].source.truncate(1);
    records.units[0].source_complete = false;
    let retained_syntax_complete = records.retained_syntax(&analysis.files);
    assert!(retained_syntax_complete);
    assert_eq!(records.objects.len(), 1);
    assert!(
        !super::publication_coverage(&analysis.files, &records, false, retained_syntax_complete)[0]
            .inventory_complete
    );
}

#[test]
fn refused_heading_missing_source_unit_keeps_inventory_incomplete() {
    let analysis = refused_heading_analysis();
    let mut records = retained_heading_records(&analysis);
    records.units.clear();
    let retained_syntax_complete = records.retained_syntax(&analysis.files);
    assert!(records.objects.is_empty());
    assert!(!retained_syntax_complete);
    assert!(
        !super::publication_coverage(&analysis.files, &records, false, retained_syntax_complete)[0]
            .inventory_complete
    );
}

#[test]
fn retained_refused_heading_requires_original_utf8_ranges() {
    let analysis = refused_heading_analysis();
    let mut symbol = analysis.files()[0].file().syntax().left_out_symbols()[0].clone();
    assert!(super::retained_declaration_ranges(
        REFUSED_HEADING_SOURCE,
        &symbol
    ));
    symbol.item_range.end = u64::MAX;
    assert!(!super::retained_declaration_ranges(
        REFUSED_HEADING_SOURCE,
        &symbol
    ));
    symbol.item_range = symbol.range;
    symbol.range.start = 1;
    symbol.range.end = 2;
    symbol.item_range = symbol.range;
    symbol.name_range = None;
    symbol.body_range = None;
    symbol.documentation_ranges.clear();
    assert!(!super::retained_declaration_ranges("é", &symbol));

    let mut analysis = refused_heading_analysis();
    let held = &mut analysis.files[0];
    held.file = crate::IndexedFile::new_with_shared_syntax(
        held.file.path().clone(),
        std::sync::Arc::new("é".to_owned()),
        held.file.digest(),
        held.file.executable(),
        std::sync::Arc::new(held.file.syntax().clone()),
    );
    let mut records = retained_heading_records(&analysis);
    let retained_syntax_complete = records.retained_syntax(&analysis.files);
    assert!(!retained_syntax_complete);
    assert!(
        !super::publication_coverage(&analysis.files, &records, false, retained_syntax_complete)[0]
            .inventory_complete
    );
}
