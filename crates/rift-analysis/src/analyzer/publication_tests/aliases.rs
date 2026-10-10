//! Explicit package entry bindings retain defining identity and physical declarations.

use super::*;

const JAVASCRIPT: &str = "function format2(value) { return value; }\nclass SemVer { format(value) { return value; } }\nexport { format2 as format };\n";
const TYPESCRIPT: &str = "export declare function format(value: string): string;\n";

fn analysis(javascript: &str, captured_metadata: bool) -> PackageAnalysis {
    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "prettier".to_owned(),
        version: "3.8.5".to_owned(),
    };
    let owner = package.owner().expect("defining package owner");
    let origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Dependency { package }),
        rift_core::SourceKind::Authored,
    )
    .expect("authored package origin");
    let language = Language::from_identity_segment("javascript").expect("language");
    let implementation_path = ProjectPath::new("index.mjs").expect("implementation path");
    let types_path = ProjectPath::new("index.d.ts").expect("declaration path");
    let metadata_path = ProjectPath::new("package.json").expect("captured metadata path");
    let sources = [
        PackageSource::new(&implementation_path, javascript),
        PackageSource::new(&types_path, TYPESCRIPT),
    ];
    let metadata = [PackageSource::new(
        &metadata_path,
        "{\"name\":\"prettier\",\"version\":\"3.8.5\",\"main\":\"index.mjs\",\"types\":\"index.d.ts\"}",
    )];
    let context = if captured_metadata {
        metadata.as_slice()
    } else {
        &[]
    };
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(3, 4096),
    )
    .expect("bounded selected package")
    .with_framework_context(context, &[])
    .expect("captured entry contract");
    PackageAnalyzer::analyze(input, 1).expect("normalized entry declarations")
}

fn identity(owner: &SymbolOwner, names: &[&str]) -> SymbolId {
    let identity = rift_protocol::identity::SymbolIdentity::new(
        owner.clone(),
        Language::from_identity_segment("javascript").expect("language"),
        names.iter().map(|name| (*name).to_owned()).collect(),
    )
    .expect("logical defining identity");
    SymbolId::parse(&identity.wire_identity()).expect("canonical wire identity")
}

#[test]
fn explicit_alias_keeps_defining_object_and_original_export_span() {
    let analysis = analysis(JAVASCRIPT, true);
    let publication = analysis.publication();
    let definition = identity(&publication.owner, &["prettier", "format2"]);
    let alias = identity(&publication.owner, &["prettier", "format"]);
    let nested = identity(&publication.owner, &["prettier", "SemVer", "format"]);
    let object = publication
        .objects
        .iter()
        .find(|object| object.id.as_ref() == Some(&definition))
        .expect("defining formatter object");
    assert_eq!(object.name, "format2");
    assert_eq!(object.language.name, "javascript");
    assert!(object.signatures.len() >= 2);
    assert_eq!(
        publication
            .objects
            .iter()
            .filter(|object| object.id.as_ref() == Some(&definition))
            .count(),
        1
    );
    assert!(
        publication
            .objects
            .iter()
            .any(|object| object.id.as_ref() == Some(&nested))
    );
    let bindings = publication
        .declarations
        .iter()
        .filter(|binding| binding.symbol == definition)
        .collect::<Vec<_>>();
    assert!(
        bindings
            .iter()
            .any(|binding| binding.unit.0.ends_with("/index.mjs"))
    );
    assert!(
        bindings
            .iter()
            .any(|binding| binding.unit.0.ends_with("/index.d.ts")
                && binding.source == "function format(value: string): string;"
                && TYPESCRIPT[usize::try_from(binding.range.start).expect("binding start")
                    ..usize::try_from(binding.range.end).expect("binding end")]
                    == binding.source)
    );
    for binding in bindings {
        assert!(binding.signature_indices.iter().all(|index| {
            usize::try_from(*index).is_ok_and(|index| index < object.signatures.len())
        }));
    }
    let edge = publication
        .relationships
        .iter()
        .find(|edge| {
            edge.from == alias
                && edge.to == definition
                && edge
                    .facets
                    .contains(&rift_protocol::read::RelationshipFacet::Aliases)
        })
        .expect("explicit alias relationship");
    assert_eq!(
        edge.derivation,
        rift_protocol::read::RelationshipDerivation::Syntax
    );
    assert!(edge.confidence.is_none());
    assert!(edge.evidence.is_empty());
    let occurrence = edge
        .occurrence
        .as_ref()
        .expect("original export occurrence");
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.unit == occurrence.unit)
        .expect("admitted physical implementation unit");
    assert_eq!(unit.path.0, "index.mjs");
    let start = usize::try_from(occurrence.range.start).expect("source start");
    let end = usize::try_from(occurrence.range.end).expect("source end");
    assert_eq!(&unit.source[start..end], "export { format2 as format };");
    assert!(
        publication
            .declarations
            .iter()
            .any(|binding| binding.symbol == alias
                && binding.unit == occurrence.unit
                && binding.range == occurrence.range)
    );
}

#[test]
fn missing_entry_contract_keeps_both_physical_languages_unresolved() {
    let analysis = analysis(JAVASCRIPT, false);
    let publication = analysis.publication();
    assert!(
        publication
            .objects
            .iter()
            .any(|object| object.name == "format2")
    );
    assert!(
        publication
            .objects
            .iter()
            .any(|object| object.name == "format")
    );
    assert!(publication.objects.iter().all(|object| object.id.is_none()));
    assert!(publication.declarations.is_empty());
    assert!(publication.relationships.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(publication.units.len(), 2);
}

#[test]
fn alias_collision_retains_definitions_without_arbitrary_target() {
    let analysis = analysis(
        "function format(value) { return value; }\nfunction format2(value) { return value; }\nexport { format2 as format };\n",
        true,
    );
    let publication = analysis.publication();
    let lexical = identity(&publication.owner, &["prettier", "format"]);
    let other = identity(&publication.owner, &["prettier", "format2"]);
    assert!(
        publication
            .objects
            .iter()
            .any(|object| object.id.as_ref() == Some(&lexical))
    );
    assert!(
        publication
            .objects
            .iter()
            .any(|object| object.id.as_ref() == Some(&other))
    );
    assert!(
        !publication
            .relationships
            .iter()
            .any(|edge| edge.from == lexical && edge.to == other)
    );
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
}
