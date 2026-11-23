//! Publication of checked TypeScript file-module imports.

use super::*;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::read::{EdgeDerivation, EdgeProvider, SourceLocation};

const MODULE: &str = "rift://symbol/npm/npmjs.org/app/typescript/app";
const SIBLING: &str = "rift://symbol/npm/npmjs.org/app/typescript/app%2Fnear";
const DEPENDENCY: &str = "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType";
const ENTRY: &str = "src/index.ts";
const NEAR: &str = "src/near.ts";
const MANIFEST: &str = r#"{"name":"app","version":"1.0.0","type":"module","exports":{".":{"types":"./src/index.ts"},"./near":{"types":"./src/near.ts"}},"dependencies":{"shape":"npm:zod@^4"}}"#;
const AMBIGUOUS: &str = r#"{"name":"app","version":"1.0.0","exports":{".":{"types":"./src/index.ts"},"./other":{"types":"./src/index.ts"}},"dependencies":{"shape":"npm:zod@^4"}}"#;
const SOURCE: &str = "// é\n\
    import type { ZodType as Imported } from \"shape\";\n\
    import type { Local as Neighbor } from \"./near\";\n\
    import type * as sibling from \"./near\";\n\
    import type { Missing } from \"undeclared\";\n\
    export type Shape = Neighbor;\n\
    export { Shape as PublicShape };\n";
const NEAR_SOURCE: &str = "export type Local = string;\n";

fn analyze_module(
    source: &str,
    metadata: &str,
    configuration: PackageConfiguration,
) -> PackageAnalysis {
    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "npmjs.org".to_owned(),
        name: "app".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("established owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let language = ShippedLanguage::TypeScript.language();
    let entry = ProjectPath::new(ENTRY).expect("entry path");
    let near = ProjectPath::new(NEAR).expect("sibling path");
    let manifest = ProjectPath::new("package.json").expect("metadata path");
    let sources = [
        PackageSource::new(&entry, source),
        PackageSource::new(&near, NEAR_SOURCE),
    ];
    let context = [PackageSource::new(&manifest, metadata)];
    let limits = ExactPackageLimits::new(3, 16_384)
        .with_publication(configuration)
        .expect("publication bounds");
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("package bytes")
        .with_framework_context(&context, &[])
        .expect("captured npm metadata");
    PackageAnalyzer::analyze(input, 1).expect("package publication")
}

#[test]
fn module_imports_keep_exact_owners_ranges_and_existing_aliases() {
    let analysis = analyze_module(SOURCE, MANIFEST, PackageConfiguration::default());
    let (publication, _, semantics, _) = analysis.into_parts();
    let mut modules = publication
        .objects
        .iter()
        .filter(|object| object.kinds.contains(&SymbolKind::Module));
    let module = modules.next().expect("checked importing module");
    assert!(modules.next().is_none());
    assert_eq!(module.id.as_ref().map(SymbolId::as_str), Some(MODULE));
    assert_eq!(module.name, "app");
    assert_eq!(module.origin.source_kind, SourceKind::Synthetic);
    assert!(module.origin.location.is_none());
    assert!(module.origin.package.is_none());
    assert!(module.origin.runtime.is_none());
    publication
        .validate_facts()
        .expect("covered published edges");
    assert!(
        publication
            .declarations
            .iter()
            .all(|declaration| declaration.symbol.as_str() != MODULE)
    );

    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == ENTRY)
        .expect("import source");
    let owner = CoreSymbolId::new(MODULE.to_owned()).expect("canonical module");
    let imports = publication
        .edges
        .iter()
        .filter(|edge| edge.from.as_str() == MODULE)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    let sibling_item = format!("{SIBLING}/Local");
    for (target, name, derivation) in [
        (DEPENDENCY, "Imported", EdgeDerivation::Syntax),
        (
            sibling_item.as_str(),
            "Neighbor",
            EdgeDerivation::Resolution,
        ),
    ] {
        let edge = imports
            .iter()
            .find(|edge| edge.to.as_str() == target)
            .expect("written named import");
        assert_eq!(edge.kinds, [EdgeKind::Imports]);
        assert_eq!(edge.provider, EdgeProvider::Static);
        assert_eq!(edge.derivation, derivation);
        let occurrence = edge.occurrence.as_ref().expect("physical occurrence");
        let start = u64::try_from(SOURCE.find(name).expect("written alias")).expect("byte offset");
        let end = start + u64::try_from(name.len()).expect("identifier bytes");
        assert_eq!(occurrence.unit, unit.unit);
        assert_eq!(occurrence.range, TextRange { start, end });
        assert!(
            semantics
                .edges()
                .outgoing(&owner)
                .iter()
                .any(|stored| &stored.to_protocol() == *edge)
        );
    }
    assert!(
        semantics
            .edges()
            .outgoing(&owner)
            .iter()
            .any(|edge| { edge.kind() == EdgeKind::Imports && edge.to().as_str() == SIBLING })
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Aliases]
            && edge.from.as_str() == format!("{MODULE}/PublicShape")
            && edge.to.as_str() == format!("{MODULE}/Shape")
    }));
}

#[test]
fn module_imports_refuse_uncertain_sources_and_publication_bounds() {
    let malformed = format!("{SOURCE}export type Broken = ;\n");
    let cases = [
        (
            malformed.as_str(),
            MANIFEST,
            PackageConfiguration::default(),
        ),
        (SOURCE, AMBIGUOUS, PackageConfiguration::default()),
        (
            "import type { ZodType as Imported } from \"shape\";\nexport type Shape = string;",
            MANIFEST,
            PackageConfiguration {
                symbols: 2,
                ..PackageConfiguration::default()
            },
        ),
    ];
    for (source, metadata, configuration) in cases {
        let analysis = analyze_module(source, metadata, configuration);
        let publication = analysis.publication();
        assert_eq!(
            publication
                .objects
                .iter()
                .filter(|object| object.id.as_ref().is_some_and(|id| id.as_str() == MODULE))
                .count(),
            0,
        );
        assert!(
            publication
                .edges
                .iter()
                .all(|edge| edge.from.as_str() != MODULE)
        );
    }
}

#[test]
fn module_imports_refuse_competing_units_and_do_not_spend_an_edge_past_its_bound() {
    let analysis = analyze_module(SOURCE, MANIFEST, PackageConfiguration::default());
    let (publication, mut files, semantics, _) = analysis.into_parts();
    let mut objects = publication.objects;
    objects.retain(|object| object.id.as_ref().is_none_or(|id| id.as_str() != MODULE));
    let mut capped = Records {
        objects: objects.clone(),
        units: publication.units.clone(),
        ..Records::default()
    };
    capped
        .edges(&publication.owner, &semantics, &files, 0)
        .expect("capped publication");
    assert_eq!(capped.objects, objects);
    assert!(capped.edges.is_empty());

    let mut omitted = Records {
        objects: objects.clone(),
        units: publication
            .units
            .iter()
            .filter(|unit| unit.path.0 != ENTRY)
            .cloned()
            .collect(),
        ..Records::default()
    };
    omitted
        .edges(&publication.owner, &semantics, &files, usize::MAX)
        .expect("omitted source publication");
    assert_eq!(omitted.objects, objects);
    assert!(
        omitted
            .edges
            .iter()
            .all(|edge| edge.from.as_str() != MODULE)
    );

    let module = CoreSymbolId::new(MODULE.to_owned()).expect("canonical module");
    let sibling = files
        .iter_mut()
        .find(|held| held.file.path().as_str() == NEAR)
        .expect("second source");
    sibling.placement = sibling
        .placement
        .clone()
        .with_identity_anchors(BTreeMap::from([(".".to_owned(), module)]));
    let mut records = Records {
        objects,
        units: publication.units,
        ..Records::default()
    };
    records
        .edges(&publication.owner, &semantics, &files, usize::MAX)
        .expect("competing source publication");
    assert!(
        records
            .objects
            .iter()
            .all(|object| object.id.as_ref().is_none_or(|id| id.as_str() != MODULE))
    );
    assert!(
        records
            .edges
            .iter()
            .all(|edge| edge.from.as_str() != MODULE)
    );
}
