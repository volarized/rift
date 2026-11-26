//! Publication of edges from checked file modules.

use super::*;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::read::{EdgeDerivation, EdgeProvider, SourceLocation};

const NON_ENTRY_ARGUMENT_PROPERTIES: &str = r#"
import { receive } from "../index.mjs";
receive({ action: "first" });
receive({ action: "second" });
const options = { action: "named" };
receive(options);
"#;
const NESTED_ARGUMENT_PROPERTIES: &str = r#"
import { receive } from "../index.mjs";
run(() => receive({ action: "first" }));
run(() => receive({ action: "second" }));
"#;

#[test]
fn package_retains_distinct_top_level_argument_properties_in_non_entry_files() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            (
                "index.mjs",
                "export function receive(value) { return value.action; }",
            ),
            ("bin/options.mjs", NON_ENTRY_ARGUMENT_PROPERTIES),
            ("bin/nested.mjs", NESTED_ARGUMENT_PROPERTIES),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
        )),
    )
    .expect("captured package");
    let publication = analysis.publication();
    let path = "bin/options.mjs";
    let analyzed = analysis
        .files()
        .iter()
        .find(|file| file.file().path().as_str() == path)
        .expect("retained file");
    assert!(analyzed.placement().logical_identity("action").is_none());
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == path)
        .expect("source unit");
    let declarations = ["first", "second", "named"].map(|value| {
        let written = format!("action: \"{value}\"");
        let start = u64::try_from(
            NON_ENTRY_ARGUMENT_PROPERTIES
                .find(&written)
                .expect("property"),
        )
        .expect("source offset");
        let matching = publication
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.unit == unit.unit
                    && declaration.range.start == start
                    && declaration.source == written
            })
            .collect::<Vec<_>>();
        let [declaration] = matching.as_slice() else {
            panic!("authored declaration for {value}");
        };
        assert!(!declaration.public);
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&declaration.symbol)
                && object.name == "action"
                && object.kind.0 == "property"
        }));
        declaration.symbol.clone()
    });
    let [first, second, named] = declarations;
    assert_ne!(first, second);
    assert_ne!(first, named);
    assert_ne!(second, named);
    assert!(named.as_str().ends_with("/options/action"));
    assert!(publication.declarations.iter().all(|declaration| {
        !declaration.unit.0.ends_with("/bin/nested.mjs")
            || !declaration.source.starts_with("action:")
    }));
}

/// An authored package with two private non-entry argument keys and older established facts.
fn argument_property_publication(retained_bytes: u64) -> Result<PackagePublication, RiftError> {
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owner");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let root = ProjectPath::new("index.mjs").expect("entry");
    let non_entry = ProjectPath::new("bin/options.mjs").expect("non-entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let files = [
        PackageSource::new(
            &root,
            "export function receive(value) { return value.action; }",
        ),
        PackageSource::new(&non_entry, NON_ENTRY_ARGUMENT_PROPERTIES),
    ];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
    )];
    let limits = ExactPackageLimits::new(3, 4096)
        .with_retained_source_bytes(192, retained_bytes)
        .expect("bounded retained records");
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
        .expect("bounded archive facts")
        .with_framework_context(&metadata, &[])
        .expect("npm manifest");
    PackageAnalyzer::analyze(input, 1).map(|analysis| analysis.into_parts().0)
}

#[test]
fn non_entry_argument_properties_keep_the_prior_retained_byte_budget() {
    let room = argument_property_publication(u64::MAX).expect("publication with room");
    let primary = room
        .declarations
        .iter()
        .filter(|row| {
            !matches!(
                row.source.as_str(),
                "action: \"first\"" | "action: \"second\""
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(primary.len(), 6);
    let bytes = room
        .units
        .iter()
        .map(|unit| unit.source.as_str())
        .chain(primary.iter().map(|row| row.source.as_str()))
        .chain(room.documents.iter().flat_map(|doc| {
            [
                doc.file_content.as_deref(),
                doc.declaration_source.as_deref(),
            ]
            .into_iter()
            .flatten()
        }))
        .map(|source| u64::try_from(source.len()).expect("captured source bytes"))
        .sum();
    let exact = argument_property_publication(bytes).unwrap_or_else(|error| {
        panic!("old publication fits exactly at {bytes} source bytes: {error:?}")
    });
    assert_eq!(exact.declarations, primary);
    assert_eq!(exact.units, room.units);
    assert_eq!(exact.documents, room.documents);
    assert_eq!(exact.edges, room.edges);
    assert_eq!(exact.objects.len(), room.objects.len());
    assert_eq!(
        exact.objects.iter().filter(|row| row.id.is_none()).count(),
        2
    );
    let generous = argument_property_publication(bytes + 192).expect("bounded surplus");
    assert_eq!(generous.declarations, room.declarations);
    assert_eq!(generous.objects, room.objects);
    assert_eq!(generous.edges, room.edges);
    assert_eq!(generous.declarations.len(), primary.len() + 2);
}

const RETURNED_LITERAL_PROPERTIES: &str = r#"
import { inspect } from "../index.mjs";
export function returned(first) {
  if (first) return { result: 31 };
  return ({ result: 42 });
}
export function read(first) {
  return returned(first).result;
}
export function established() {
  const options = { stable: 8 };
  return inspect(options.stable);
}
export function unsupported(first) {
  return first ? { opaque: 5 } : { opaque: 6 };
}
"#;

#[test]
fn package_retains_private_properties_of_distinct_returned_literals() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            (
                "index.mjs",
                "export function inspect(value) { return value; }",
            ),
            ("lib/branches.mjs", RETURNED_LITERAL_PROPERTIES),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
        )),
    )
    .expect("captured package");
    let publication = analysis.publication();
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "lib/branches.mjs")
        .expect("non-entry unit")
        .unit;
    let property = |written: &str| {
        publication.declarations.iter().find(|row| {
            &row.unit == unit
                && row.source == written
                && u64::try_from(
                    RETURNED_LITERAL_PROPERTIES
                        .find(written)
                        .expect("source key"),
                ) == Ok(row.range.start)
        })
    };
    let first = property("result: 31").expect("first returned property");
    let second = property("result: 42").expect("second returned property");
    assert_ne!(first.symbol, second.symbol);
    for row in [first, second] {
        assert!(!row.public);
        assert!(row.symbol.as_str().contains("/returned/result%23"));
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.name == "result"
                && object.kind.0 == "property"
        }));
        assert!(
            !publication
                .edges
                .iter()
                .any(|edge| edge.kinds.contains(&EdgeKind::Reads) && edge.to == row.symbol)
        );
    }
    let stable = property("stable: 8").expect("named control");
    assert!(
        stable
            .symbol
            .as_str()
            .ends_with("/established/options/stable")
    );
    assert!(property("opaque: 5").is_none());
    assert!(property("opaque: 6").is_none());
}

const MODULE: &str = "rift://symbol/npm/npmjs.org/app/typescript/app";
const SIBLING: &str = "rift://symbol/npm/npmjs.org/app/typescript/app%2Fnear";
const DEPENDENCY: &str = "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType";
const ENTRY: &str = "src/index.ts";
const NEAR: &str = "src/near.ts";
const MANIFEST: &str = r#"{"name":"app","version":"1.0.0","type":"module","exports":{".":{"types":"./src/index.ts"},"./near":{"types":"./src/near.ts"}},"dependencies":{"shape":"npm:zod@^4"}}"#;
const SHARED_ROOT: &str = r#"{"name":"app","version":"1.0.0","type":"module","exports":{"./other":{"types":"./src/index.ts"},"./near":{"types":"./src/near.ts"},".":{"types":"./src/index.ts"}},"dependencies":{"shape":"npm:zod@^4"}}"#;
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
    let modules = publication
        .objects
        .iter()
        .filter(|object| object.kinds.contains(&SymbolKind::Module))
        .collect::<Vec<_>>();
    assert_eq!(modules.len(), 2);
    let module = modules
        .iter()
        .find(|object| object.id.as_ref().map(SymbolId::as_str) == Some(MODULE))
        .expect("checked importing module");
    assert_eq!(module.id.as_ref().map(SymbolId::as_str), Some(MODULE));
    assert_eq!(module.name, "app");
    assert_eq!(module.origin.source_kind, SourceKind::Authored);
    assert!(module.origin.location.is_some());
    assert!(module.origin.package.is_some());
    assert!(module.origin.runtime.is_none());
    publication
        .validate_facts()
        .expect("covered published edges");
    assert!(
        publication.declarations.iter().any(
            |declaration| declaration.symbol.as_str() == MODULE && declaration.source == SOURCE
        )
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
    assert_eq!(imports.len(), 4);
    let written = "\"./near\"";
    for (start, _) in SOURCE.match_indices(written) {
        let start = u64::try_from(start).expect("module specifier position");
        let end = start + u64::try_from(written.len()).expect("module specifier length");
        assert!(imports.iter().any(|edge| {
            edge.to.as_str() == SIBLING
                && edge.kinds == [EdgeKind::Imports]
                && edge.provider == EdgeProvider::Static
                && edge.derivation == EdgeDerivation::Resolution
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == unit.unit && occurrence.range == TextRange { start, end }
                })
        }));
    }
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
fn module_imports_keep_the_same_authored_root_when_an_alias_selects_it() {
    let single = analyze_module(SOURCE, MANIFEST, PackageConfiguration::default());
    let aliased = analyze_module(SOURCE, SHARED_ROOT, PackageConfiguration::default());
    let single = single.publication();
    let aliased = aliased.publication();
    let expected = single
        .edges
        .iter()
        .filter(|row| row.from.as_str() == MODULE)
        .collect::<Vec<_>>();
    assert!(!expected.is_empty());
    let found = aliased
        .edges
        .iter()
        .filter(|row| row.from.as_str() == MODULE)
        .collect::<Vec<_>>();
    assert_eq!(found, expected);

    let shape = format!("{MODULE}/Shape");
    let declaration = single
        .declarations
        .iter()
        .find(|row| row.symbol.as_str() == shape);
    assert!(declaration.is_some());
    assert_eq!(
        aliased
            .declarations
            .iter()
            .find(|row| row.symbol.as_str() == shape),
        declaration,
    );
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

fn analyze_javascript_module(
    mode: &str,
    entry: &str,
    entry_source: &str,
    dependency: &str,
    dependency_source: &str,
) -> PackageAnalysis {
    let metadata =
        format!(r#"{{"name":"beacon","version":"1.0.0","type":"{mode}","main":"./{entry}"}}"#);
    super::internal_modules::analyze(
        &metadata,
        &[(entry, entry_source), (dependency, dependency_source)],
    )
}

#[test]
fn javascript_file_module_calls_keep_the_physical_entry_and_target() {
    const MODULE: &str = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    for (mode, suffix, source, target_text, import) in [
        (
            "module",
            "mjs",
            concat!(
                "import { alpha } from './left.mjs';\n",
                "alpha();\n",
                "unknown(() => alpha());\n",
                "export function make() { alpha(); }\n",
            ),
            "export function alpha() {}\n",
            true,
        ),
        (
            "commonjs",
            "cjs",
            concat!(
                "const { alpha } = require('./left.cjs');\n",
                "alpha();\n",
                "unknown(() => alpha());\n",
                "exports.make = function () { alpha(); };\n",
            ),
            "exports.alpha = function () {};\n",
            false,
        ),
    ] {
        let entry = format!("index.{suffix}");
        let dependency = format!("left.{suffix}");
        let analysis = analyze_javascript_module(mode, &entry, source, &dependency, target_text);
        let (found, _, semantics, _) = analysis.into_parts();
        found.validate_facts().expect("valid module publication");
        let unit = found
            .units
            .iter()
            .find(|unit| unit.path.0 == entry)
            .expect("entry exists");
        let target = format!("{MODULE}%2F{dependency}/alpha");
        assert_javascript_module_declaration(
            &found,
            MODULE,
            &target,
            &dependency,
            target_text,
            import,
        );
        let calls = found
            .edges
            .iter()
            .filter(|row| row.from.as_str() == MODULE && row.kinds == [EdgeKind::Calls])
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1, "{suffix} {calls:?}");
        assert_eq!(calls[0].to.as_str(), target);
        let call = calls[0].occurrence.as_ref().expect("authored call");
        let start = u64::try_from(source.find("\nalpha()").expect("top-level call") + 1)
            .expect("byte offset");
        assert_eq!(call.unit, unit.unit);
        assert_eq!(
            call.range,
            TextRange {
                start,
                end: start + 7
            }
        );
        let owner = CoreSymbolId::new(MODULE).expect("canonical owner");
        assert_eq!(
            semantics
                .edges()
                .outgoing(&owner)
                .iter()
                .find(|row| &row.to_protocol() == calls[0])
                .and_then(crate::edge::Edge::static_callee),
            Some(rift_syntax::ByteRange {
                start,
                end: start + 5,
            })
        );
        assert!(found.edges.iter().any(|row| {
            row.from.as_str() == format!("{MODULE}/make")
                && row.to.as_str() == target
                && row.kinds == [EdgeKind::Calls]
        }));
        assert_javascript_module_imports(&found, MODULE, &target, source, &unit.unit, import);
    }
}

fn assert_javascript_module_declaration(
    found: &PackagePublication,
    module: &str,
    target: &str,
    dependency: &str,
    target_text: &str,
    import: bool,
) {
    let target_source = found
        .units
        .iter()
        .find(|unit| unit.path.0 == dependency)
        .expect("target source exists");
    let declaration = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str() == target)
        .expect("authored target declaration");
    assert_eq!(declaration.unit, target_source.unit);
    let declaration_start = if import { 7 } else { 0 };
    let declaration_end = u64::try_from(target_text.trim_end_matches([';', '\n']).len())
        .expect("target source bytes");
    assert_eq!(
        declaration.range,
        TextRange {
            start: declaration_start,
            end: declaration_end,
        }
    );
    let object = found
        .objects
        .iter()
        .find(|object| object.id.as_ref().is_some_and(|id| id.as_str() == module))
        .expect("checked JavaScript module");
    assert_eq!(object.kinds, [SymbolKind::Module]);
    assert_eq!(object.origin.source_kind, SourceKind::Authored);
    assert!(
        found
            .declarations
            .iter()
            .any(|row| { row.symbol.as_str() == module && row.unit != target_source.unit })
    );
}

fn assert_javascript_module_imports(
    found: &PackagePublication,
    module: &str,
    target: &str,
    source: &str,
    unit: &SourceUnitId,
    import: bool,
) {
    let written_imports = found
        .edges
        .iter()
        .filter(|row| row.from.as_str() == module && row.kinds == [EdgeKind::Imports])
        .collect::<Vec<_>>();
    if import {
        let named_import = written_imports
            .iter()
            .filter(|row| row.to.as_str() == target)
            .collect::<Vec<_>>();
        assert_eq!(named_import.len(), 1, "{written_imports:?}");
        let binding = named_import[0].occurrence.as_ref().expect("written import");
        let start =
            u64::try_from(source.find("alpha").expect("imported name")).expect("byte offset");
        assert_eq!(&binding.unit, unit);
        assert_eq!(
            binding.range,
            TextRange {
                start,
                end: start + 5
            }
        );
    } else {
        assert_eq!(written_imports.len(), 1, "{written_imports:?}");
        let module_target = target.strip_suffix("/alpha").expect("required module");
        assert_eq!(written_imports[0].to.as_str(), module_target);
        let binding = written_imports[0]
            .occurrence
            .as_ref()
            .expect("written module");
        let specifier = "'./left.cjs'";
        assert_eq!(&binding.unit, unit);
        assert_eq!(
            binding.range,
            module_runtime_range(source.find(specifier).expect("module specifier"), specifier)
        );
        assert!(found.edges.iter().any(|row| {
            row.from.as_str() == format!("{module}/alpha")
                && row.to.as_str() == target
                && row.kinds == [EdgeKind::Imports]
        }));
    }
}

#[test]
fn javascript_file_module_calls_do_not_create_a_legacy_script_owner() {
    const MODULE: &str = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    let analysis = analyze_javascript_module(
        "commonjs",
        "index.js",
        "function alpha() {}\nalpha();\n",
        "unused.js",
        "function beta() {}\n",
    );
    let found = analysis.publication();
    assert!(
        found
            .objects
            .iter()
            .all(|object| object.id.as_ref().is_none_or(|id| id.as_str() != MODULE))
    );
    assert!(found.edges.iter().all(|row| row.from.as_str() != MODULE));
}

#[test]
fn javascript_file_module_calls_do_not_displace_typescript_module_imports() {
    const JS: &str = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    const TS: &str = "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            (
                "index.mjs",
                "import { alpha } from './left.mjs';\nalpha();\nexport function make() { alpha(); }\n",
            ),
            ("left.mjs", "export function alpha() {}\n"),
            (
                "index.d.ts",
                "import { Shape } from './left';\nexport type Local = Shape;\n",
            ),
            ("left.d.ts", "export interface Shape { label: string; }\n"),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs","types":"./index.d.ts"}"#,
        )),
    )
    .expect("mixed npm package");
    let (publication, files, semantics, _) = analysis.into_parts();
    let objects = publication
        .objects
        .into_iter()
        .filter(|object| {
            object
                .id
                .as_ref()
                .is_none_or(|id| !matches!(id.as_str(), JS | TS))
        })
        .collect::<Vec<_>>();
    let mut uncapped = Records {
        objects: objects.clone(),
        units: publication.units.clone(),
        ..Records::default()
    };
    uncapped
        .edges(&publication.owner, &semantics, &files, usize::MAX)
        .expect("all graph edges");
    assert!(
        uncapped
            .edges
            .iter()
            .any(|row| { row.from.as_str() == JS && row.kinds == [EdgeKind::Calls] })
    );
    assert!(
        uncapped
            .edges
            .iter()
            .any(|row| { row.from.as_str() == TS && row.kinds == [EdgeKind::Imports] })
    );
    let earlier = uncapped
        .edges
        .iter()
        .filter(|row| !matches!(row.from.as_str(), JS | TS))
        .count();
    let mut capped = Records {
        objects,
        units: publication.units,
        ..Records::default()
    };
    capped
        .edges(&publication.owner, &semantics, &files, earlier + 1)
        .expect("one place for a synthetic module edge");
    assert!(
        capped
            .edges
            .iter()
            .any(|row| { row.from.as_str() == TS && row.kinds == [EdgeKind::Imports] })
    );
    assert!(capped.edges.iter().all(|row| row.from.as_str() != JS));
}

#[test]
fn npm_file_modules_keep_only_direct_captured_runtime_occurrences() {
    const DEPENDENCY: &str =
        "export function leaf() { return 1; }\nexport const state = { value: 0 };\n";
    let cases = [
        (ShippedLanguage::JavaScript, "javascript", "mjs", "main"),
        (ShippedLanguage::TypeScript, "typescript", "ts", "types"),
    ];
    for (shipped, language, extension, selector) in cases {
        let entry = format!("index.{extension}");
        let leaf = format!("leaf.{extension}");
        let source = format!(
            r#"import {{ leaf, state }} from "./{leaf}";
leaf();
state.value;
state.value = 3;
const owned = leaf();
const held = () => {{ leaf(); state.value; }};
unknown(() => {{ leaf(); state.value; }});
export function run() {{ leaf(); return state.value; }}
export {{ owned, held }};
"#,
        );
        let metadata = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"module","{selector}":"./{entry}"}}"#,
        );
        let analysis = fixture::package_result_with_metadata(
            shipped,
            vec![(&entry, &source), (&leaf, DEPENDENCY)],
            None,
            Some(("package.json", &metadata)),
        )
        .expect("captured npm package");
        let (publication, files, semantics, _) = analysis.into_parts();
        publication.validate_facts().expect("valid retained facts");
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == entry)
            .expect("physical entry");
        let module = format!("rift://symbol/npm/registry.npmjs.org/beacon/{language}/beacon");
        let target_base = format!("{module}%2F{leaf}");
        let direct_end = source.find("const owned").expect("first declaration");
        assert_direct_module_runtime_roles(
            &publication,
            &unit.unit,
            &source,
            &module,
            &target_base,
            language,
        );
        assert!(publication.edges.iter().all(|edge| {
            edge.from.as_str() != module
                || !matches!(
                    edge.kinds.as_slice(),
                    [EdgeKind::Calls | EdgeKind::Reads | EdgeKind::Writes]
                )
                || edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.range.end <= u64::try_from(direct_end).expect("entry boundary")
                })
        }));
        for (prefix, owner) in [
            ("const owned", "owned"),
            ("const held", "held"),
            ("export function run", "run"),
        ] {
            let statement = source.find(prefix).expect("owning declaration");
            let at = statement + source[statement..].find("leaf()").expect("owned call");
            assert!(publication.edges.iter().any(|edge| {
                edge.from.as_str() == format!("{module}/{owner}")
                    && edge.kinds == [EdgeKind::Calls]
                    && edge.occurrence.as_ref().is_some_and(|occurrence| {
                        occurrence.unit == unit.unit
                            && occurrence.range == module_runtime_range(at, "leaf()")
                    })
            }));
        }
        assert_new_runtime_roles_follow_prior_package_edges(
            publication,
            &files,
            &semantics,
            &module,
            language,
        );
    }
}

fn module_runtime_range(at: usize, spelling: &str) -> TextRange {
    TextRange {
        start: u64::try_from(at).expect("authored byte offset"),
        end: u64::try_from(at + spelling.len()).expect("authored byte offset"),
    }
}

fn assert_direct_module_runtime_roles(
    publication: &PackagePublication,
    unit: &SourceUnitId,
    source: &str,
    module: &str,
    target_base: &str,
    language: &str,
) {
    for (role, located, spelling, target) in [
        (EdgeKind::Calls, "leaf();\n", "leaf()", "leaf"),
        (
            EdgeKind::Reads,
            "state.value;\n",
            "state.value",
            "state/value",
        ),
        (
            EdgeKind::Writes,
            "state.value = 3",
            "state.value",
            "state/value",
        ),
    ] {
        let at = source.find(located).expect("direct occurrence");
        let edges = publication
            .edges
            .iter()
            .filter(|edge| {
                edge.from.as_str() == module
                    && edge.kinds == [role]
                    && edge.to.as_str() == format!("{target_base}/{target}")
                    && edge.occurrence.as_ref().is_some_and(|occurrence| {
                        &occurrence.unit == unit
                            && occurrence.range == module_runtime_range(at, spelling)
                    })
            })
            .count();
        assert_eq!(edges, 1, "{language} {role:?}: {spelling}");
    }
}

fn assert_new_runtime_roles_follow_prior_package_edges(
    publication: PackagePublication,
    files: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    module: &str,
    language: &str,
) {
    let module_references = semantics
        .edges()
        .edges()
        .filter(|edge| edge.is_file_module_reference())
        .map(crate::Edge::to_protocol)
        .collect::<Vec<_>>();
    let available_before = publication
        .edges
        .iter()
        .filter(|edge| !module_references.contains(edge))
        .filter(|edge| {
            edge.from.as_str() != module
                || match edge.kinds.as_slice() {
                    [EdgeKind::Calls] => language == "javascript",
                    [EdgeKind::Reads | EdgeKind::Writes] => false,
                    _ => true,
                }
        })
        .collect::<Vec<_>>();
    let mut capped = Records {
        objects: publication
            .objects
            .into_iter()
            .filter(|object| object.id.as_ref().is_none_or(|id| id.as_str() != module))
            .collect(),
        units: publication.units,
        ..Records::default()
    };
    capped
        .edges(&publication.owner, semantics, files, available_before.len())
        .expect("existing module edges fit the previous complete capacity");
    assert_eq!(capped.edges.len(), available_before.len());
    assert!(
        available_before
            .iter()
            .all(|edge| capped.edges.contains(edge))
    );
}

#[test]
fn npm_complete_file_modules_can_publish_without_outgoing_edges() {
    for (shipped, language, entry, mode, source, admitted) in [
        (
            ShippedLanguage::JavaScript,
            "javascript",
            "index.mjs",
            "module",
            "export {};\n",
            true,
        ),
        (
            ShippedLanguage::TypeScript,
            "typescript",
            "index.mts",
            "module",
            "export {};\n",
            true,
        ),
        (
            ShippedLanguage::JavaScript,
            "javascript",
            "index.cjs",
            "commonjs",
            "\n",
            true,
        ),
        (
            ShippedLanguage::JavaScript,
            "javascript",
            "index.js",
            "commonjs",
            "function local() {}\n",
            false,
        ),
        (
            ShippedLanguage::JavaScript,
            "javascript",
            "index.mjs",
            "module",
            "export const = ;\n",
            false,
        ),
    ] {
        let selector = if shipped == ShippedLanguage::TypeScript {
            "types"
        } else {
            "main"
        };
        let metadata = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"{mode}","{selector}":"./{entry}"}}"#,
        );
        let analysis = fixture::package_result_with_metadata(
            shipped,
            vec![(entry, source)],
            None,
            Some(("package.json", &metadata)),
        )
        .expect("retained selected module bytes");
        let (publication, files, semantics, _) = analysis.into_parts();
        publication
            .validate_facts()
            .expect("valid file module facts");
        assert!(publication.edges.is_empty());
        assert!(
            publication.units.iter().any(|unit| {
                unit.path.0 == entry && unit.source == source && unit.source_complete
            })
        );
        let id = format!("rift://symbol/npm/registry.npmjs.org/beacon/{language}/beacon");
        let symbols = publication
            .objects
            .iter()
            .filter(|object| {
                object
                    .id
                    .as_ref()
                    .is_some_and(|symbol| symbol.as_str() == id)
            })
            .collect::<Vec<_>>();
        assert_eq!(symbols.len(), usize::from(admitted), "{entry} {source:?}");
        if admitted {
            assert_eq!(symbols[0].kinds, [SymbolKind::Module]);
            assert_eq!(symbols[0].origin.source_kind, SourceKind::Synthetic);
            assert!(publication.declarations.is_empty());
            assert_empty_module_respects_the_symbol_bound(publication, &files, &semantics);
        }
    }
}

#[test]
fn npm_written_file_modules_publish_their_own_source() {
    const MAIN: &str = "import { leaf } from './leaf.mjs';\nexport function run() { leaf(); }\n";
    const LEAF: &str = "export function leaf() {}\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", MAIN), ("leaf.mjs", LEAF)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("authored package modules");
    let publication = analysis.publication();
    publication.validate_facts().expect("valid package modules");
    for (path, suffix, source) in [
        ("index.mjs", "/beacon", MAIN),
        ("leaf.mjs", "/beacon%2Fleaf.mjs", LEAF),
    ] {
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("source unit");
        let object = publication
            .objects
            .iter()
            .find(|object| {
                object.kinds.contains(&SymbolKind::Module)
                    && object
                        .id
                        .as_ref()
                        .is_some_and(|identity| identity.as_str().ends_with(suffix))
            })
            .expect("file module");
        assert_eq!(object.origin.source_kind, SourceKind::Authored);
        let module = object.id.as_ref().expect("module identity");
        let row = publication
            .declarations
            .iter()
            .find(|row| row.symbol == *module)
            .expect("physical module");
        assert_eq!(row.unit, unit.unit);
        assert_eq!(row.range.start, 0);
        assert_eq!(
            row.range.end,
            u64::try_from(source.len()).expect("source length")
        );
        assert_eq!(row.source, source);
        assert!(row.source_complete);
    }
    let specifier = "'./leaf.mjs'";
    let start = u64::try_from(MAIN.find(specifier).expect("written module")).expect("byte offset");
    let end = start + u64::try_from(specifier.len()).expect("specifier bytes");
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.mjs")
        .expect("module unit");
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/beacon")
            && edge.to.as_str().ends_with("/beacon%2Fleaf.mjs")
            && edge.kinds == [EdgeKind::Imports]
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.unit == unit.unit && site.range == TextRange { start, end }
            })
    }));
}

fn assert_empty_module_respects_the_symbol_bound(
    publication: PackagePublication,
    files: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
) {
    let mut bounded = Records {
        units: publication.units,
        publication: PackageConfiguration {
            symbols: 0,
            ..PackageConfiguration::default()
        },
        ..Records::default()
    };
    bounded
        .edges(&publication.owner, semantics, files, usize::MAX)
        .expect("bounded file module publication");
    assert!(bounded.objects.is_empty());
    assert!(bounded.symbols_truncated);
}

#[test]
fn empty_file_modules_refuse_a_competing_selected_source() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", "export {};\n"), ("other.mjs", "export {};\n")],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("captured sibling modules");
    let (publication, mut files, semantics, _) = analysis.into_parts();
    let module = files[0]
        .placement
        .logical_identity(".")
        .cloned()
        .expect("checked entry owner");
    files[1].placement = files[1]
        .placement
        .clone()
        .with_identity_anchors(BTreeMap::from([(".".to_owned(), module.clone())]));
    let mut records = Records {
        units: publication.units,
        ..Records::default()
    };
    records
        .edges(&publication.owner, &semantics, &files, usize::MAX)
        .expect("competing source publication");
    assert!(records.objects.iter().all(|object| {
        object
            .id
            .as_ref()
            .is_none_or(|id| id.as_str() != module.as_str())
    }));
    assert!(records.edges.is_empty());
}

#[test]
fn empty_file_module_objects_follow_existing_module_edge_owners_at_the_cap() {
    const ENTRY: &str = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    const WORKER: &str =
        "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon%2Fworker.mjs";

    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.mjs", "export {};\n"),
            (
                "worker.mjs",
                "import { leaf } from './leaf.mjs';\nleaf();\nexport function run() { leaf(); }\n",
            ),
            ("leaf.mjs", "export function leaf() {}\n"),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("captured independent file owners");
    let (publication, files, semantics, _) = analysis.into_parts();
    assert!(
        publication
            .objects
            .iter()
            .any(|object| { object.id.as_ref().is_some_and(|id| id.as_str() == ENTRY) })
    );
    let declared = publication
        .objects
        .into_iter()
        .filter(|object| !object.kinds.contains(&SymbolKind::Module))
        .collect::<Vec<_>>();
    let mut capped = Records {
        publication: PackageConfiguration {
            symbols: u32::try_from(declared.len() + 1).expect("at most one module object"),
            ..PackageConfiguration::default()
        },
        objects: declared,
        units: publication.units,
        ..Records::default()
    };
    capped
        .edges(&publication.owner, &semantics, &files, usize::MAX)
        .expect("prior worker module retained within the bound");
    assert!(
        capped
            .objects
            .iter()
            .any(|object| { object.id.as_ref().is_some_and(|id| id.as_str() == WORKER) })
    );
    assert!(
        capped
            .edges
            .iter()
            .any(|edge| { edge.from.as_str() == WORKER && edge.kinds == [EdgeKind::Calls] })
    );
    assert!(
        capped
            .objects
            .iter()
            .all(|object| { object.id.as_ref().is_none_or(|id| id.as_str() != ENTRY) })
    );
    assert!(capped.symbols_truncated);
}

#[test]
fn npm_commonjs_module_reads_follow_the_written_binding() {
    const SOURCE: &str = concat!(
        "const neighbor = require('./neighbor.js');\n",
        "const replaced = require('./neighbor.js');\n",
        "replaced = { value: 5 };\n",
        "exports.result = function result() { return exports.answer + neighbor.value; };\n",
        "exports.answer = 3;\n",
        "function shadow(exports, neighbor) { return exports.answer + neighbor.value; }\n",
        "replaced.value;\n",
    );
    let analysis = analyze_javascript_module(
        "commonjs",
        "index.js",
        SOURCE,
        "neighbor.js",
        "exports.value = 4;\n",
    );
    let found = analysis.publication();
    found.validate_facts().expect("valid module publication");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.js")
        .expect("checked CommonJS unit");
    let module = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    let neighbor = format!("{module}%2Fneighbor.js");
    for target in [module, neighbor.as_str()] {
        assert!(found.declarations.iter().any(|row| {
            row.symbol.as_str() == target && row.range.start == 0 && row.source_complete
        }));
    }
    let reads_module = |start: usize, spelling: &str, target: &str| {
        found.edges.iter().any(|edge| {
            edge.to.as_str() == target
                && edge.kinds == [EdgeKind::Reads]
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == unit.unit
                        && occurrence.range == module_runtime_range(start, spelling)
                })
        })
    };
    let top = SOURCE
        .find("exports.result")
        .expect("written module receiver");
    let read = SOURCE.find("exports.answer").expect("written own export");
    let sibling = SOURCE
        .find("neighbor.value")
        .expect("written imported module");
    assert!(reads_module(top, "exports", module));
    assert!(reads_module(read, "exports", module));
    assert!(reads_module(sibling, "neighbor", &neighbor));
    let shadow = SOURCE.rfind("exports.answer").expect("shadowed exports");
    let shadow_neighbor = SOURCE.rfind("neighbor.value").expect("shadowed namespace");
    let replaced = SOURCE.find("replaced.value").expect("reassigned namespace");
    assert!(!reads_module(shadow, "exports", module));
    assert!(!reads_module(shadow_neighbor, "neighbor", &neighbor));
    assert!(!reads_module(replaced, "replaced", &neighbor));
}

#[test]
fn npm_constructor_calls_an_imported_stable_class_value() {
    const SOURCE: &str = concat!(
        "import { Target, Variable } from './target.mjs';\n",
        "var Holder = class {\n",
        "  constructor() {\n",
        "    this.target = new Target();\n",
        "    this.variable = new Variable();\n",
        "  }\n",
        "};\n",
        "export { Holder };\n",
    );
    const TARGET: &str = concat!(
        "var Target = class { constructor() { this.name = 'target'; } };\n",
        "var Variable = class {};\n",
        "Variable = function () {};\n",
        "export { Target, Variable };\n",
    );
    let analysis = analyze_javascript_module("module", "index.mjs", SOURCE, "target.mjs", TARGET);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("valid constructor references");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.mjs")
        .expect("checked constructor unit");
    let module = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    let from = format!("{module}/Holder/constructor");
    for (written, suffix, expected) in [
        ("new Target()", "Target", true),
        ("new Variable()", "Variable", false),
    ] {
        let target = format!("{module}%2Ftarget.mjs/{suffix}");
        assert!(
            found
                .declarations
                .iter()
                .any(|row| row.symbol.as_str() == target)
        );
        let start = SOURCE.find(written).expect("written construction");
        assert_eq!(
            found.edges.iter().any(|edge| {
                edge.from.as_str() == from
                    && edge.to.as_str() == target
                    && edge.kinds == [EdgeKind::Calls]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == unit.unit && site.range == module_runtime_range(start, written)
                    })
            }),
            expected,
            "{written}"
        );
    }
}

const NAMED_ARGUMENT_PROPERTIES: &str = r#"
import { consume } from "../index.mjs";
export function choose() {
  consume({ status: "ready" });
  consume({ status: "completed" });
  const first = consume({ status: "assigned first" });
  const second = consume({ status: "assigned second" });
  const options = { status: "stable" };
  return [first, second, consume(options)];
}
export function defaults(first = consume({ hidden: 1 }), second = consume({ hidden: 2 })) {
  return [first, second];
}
export function callbacks(run) {
  run(() => consume({ hidden: 3 }));
  run(() => consume({ hidden: 4 }));
}
"#;

#[test]
fn package_retains_named_call_argument_properties_without_moving_assigned_keys() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            (
                "index.mjs",
                "export function consume(value) { return value.status; }",
            ),
            ("lib/choose.mjs", NAMED_ARGUMENT_PROPERTIES),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
        )),
    )
    .expect("captured package");
    let publication = analysis.publication();
    let held = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "lib/choose.mjs")
        .expect("non-entry unit");
    let property = |value: &str| {
        let written = format!("status: \"{value}\"");
        let start = u64::try_from(
            NAMED_ARGUMENT_PROPERTIES
                .find(&written)
                .expect("written key"),
        )
        .expect("source position");
        let row = publication
            .declarations
            .iter()
            .find(|row| row.unit == held.unit && row.range.start == start && row.source == written)
            .expect("physical property");
        assert!(!row.public);
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.name == "status"
                && object.kind.0 == "property"
        }));
        row.symbol.as_str()
    };
    let first = property("ready");
    let second = property("completed");
    assert_ne!(first, second);
    assert!(first.contains("/choose/status%23"));
    assert!(second.contains("/choose/status%23"));
    assert!(property("assigned first").ends_with("/choose/first/status"));
    assert!(property("assigned second").ends_with("/choose/second/status"));
    assert!(property("stable").ends_with("/choose/options/status"));
    assert!(publication.declarations.iter().all(|row| {
        row.unit != held.unit
            || !row.source.starts_with("hidden:")
            || !row.symbol.as_str().contains("%23")
    }));
    let calls = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.from.as_str().ends_with("/choose")
                && edge.to.as_str().ends_with("/consume")
                && edge.kinds.contains(&EdgeKind::Calls)
        })
        .map(|edge| {
            let occurrence = edge.occurrence.as_ref().expect("authored invocation");
            assert_eq!(occurrence.unit, held.unit);
            offset_in(occurrence.range.start)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        calls,
        [
            "consume({ status: \"ready\" })",
            "consume({ status: \"completed\" })",
            "consume({ status: \"assigned first\" })",
            "consume({ status: \"assigned second\" })",
            "consume(options)",
        ]
        .into_iter()
        .map(|call| NAMED_ARGUMENT_PROPERTIES
            .find(call)
            .expect("written invocation"))
        .collect(),
    );
}

#[test]
fn non_entry_literal_contains_uses_only_the_capacity_left_by_existing_edges() {
    const INPUT: &str = r#"
import { inspect } from "../index.mjs";
export function choose() {
  inspect({ code: "first" });
  inspect({ code: "second" });
  const options = { code: "named" };
  return inspect(options);
}
"#;
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owner");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let root = ProjectPath::new("index.mjs").expect("entry");
    let non_entry = ProjectPath::new("lib/choose.mjs").expect("non-entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let files = [
        PackageSource::new(
            &root,
            "export function inspect(value) { return value.code; }",
        ),
        PackageSource::new(&non_entry, INPUT),
    ];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
    )];
    let publication = |edge_cap| {
        let limits = ExactPackageLimits::new(3, 4096)
            .with_edges(edge_cap)
            .expect("bounded edges");
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
            .expect("bounded archive facts")
            .with_framework_context(&metadata, &[])
            .expect("npm manifest");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("publication");
        analysis.into_parts().0
    };
    let bounded = publication(8);
    assert_prior_literal_edges(&bounded, INPUT);
    let roomy = publication(64);
    for prior in &bounded.edges {
        assert!(roomy.edges.contains(prior));
    }
    let optional = roomy
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Contains] && edge.to.as_str().contains("/choose/code%23")
        })
        .map(|edge| {
            offset_in(
                edge.occurrence
                    .as_ref()
                    .expect("literal property")
                    .range
                    .start,
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(optional, BTreeSet::from([80, 110]));
}

fn assert_prior_literal_edges(bounded: &PackagePublication, input: &str) {
    let expected = [
        (
            EdgeKind::Contains,
            "/choose",
            "/choose/options",
            "options",
            137,
        ),
        (
            EdgeKind::Reads,
            "/choose",
            "/choose/options",
            "options",
            183,
        ),
        (
            EdgeKind::Calls,
            "/choose",
            "/inspect",
            r#"inspect({ code: "first" })"#,
            70,
        ),
        (
            EdgeKind::Calls,
            "/choose",
            "/inspect",
            r#"inspect({ code: "second" })"#,
            100,
        ),
        (
            EdgeKind::Calls,
            "/choose",
            "/inspect",
            "inspect(options)",
            175,
        ),
        (
            EdgeKind::Imports,
            "%2Fchoose.mjs",
            "/inspect",
            "inspect",
            10,
        ),
    ];
    assert_eq!(bounded.edges.len(), expected.len());
    for (kind, from, to, text, start) in expected {
        assert!(
            bounded.edges.iter().any(|edge| {
                edge.kinds == [kind]
                    && edge.from.as_str().ends_with(from)
                    && edge.to.as_str().ends_with(to)
                    && edge.occurrence.as_ref().is_some_and(|occurrence| {
                        occurrence.unit.0.ends_with("/lib/choose.mjs")
                            && offset_in(occurrence.range.start) == start
                            && input.get(start..offset_in(occurrence.range.end)) == Some(text)
                    })
            }),
            "{kind:?} {from} {to} {text}"
        );
    }
}

const SCOPED_INITIALIZER_PROPERTIES: &str = r#"
import { inspect } from "../index.mjs";
export function initialize(first) {
  if (first) {
    const record = { code: 11 };
    return inspect(record.code);
  }
  const record = { code: 22 };
  const named = { code: 44 };
  inspect(named.code);
  return inspect(record.code);
}
"#;

#[test]
fn package_keeps_initializer_properties_under_their_physical_lexical_bindings() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            (
                "index.mjs",
                "export function inspect(value) { return value; }",
            ),
            ("lib/branches.mjs", SCOPED_INITIALIZER_PROPERTIES),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
        )),
    )
    .expect("captured package");
    let publication = analysis.publication();
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "lib/branches.mjs")
        .expect("non-entry unit")
        .unit;
    let declaration = |written: &str| {
        let start = SCOPED_INITIALIZER_PROPERTIES
            .find(written)
            .expect("authored declaration");
        publication
            .declarations
            .iter()
            .find(|row| {
                &row.unit == unit && offset_in(row.range.start) == start && row.source == written
            })
            .expect("retained declaration")
    };
    let owners = ["record = { code: 11 }", "record = { code: 22 }"].map(declaration);
    let properties = ["code: 11", "code: 22"].map(declaration);
    assert_ne!(owners[0].symbol, owners[1].symbol);
    assert_ne!(properties[0].symbol, properties[1].symbol);
    for (owner, property) in owners.into_iter().zip(properties) {
        assert!(!owner.public && !property.public);
        assert!(owner.symbol.as_str().contains("/initialize/record%23"));
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&property.symbol)
                && object.container.as_ref() == Some(&owner.symbol)
                && object.name == "code"
        }));
    }
    let unrelated = declaration("code: 44");
    assert!(
        unrelated
            .symbol
            .as_str()
            .ends_with("/initialize/named/code")
    );
}

#[test]
fn npm_javascript_callbacks_keep_physical_parameters_and_reads() {
    const SOURCE: &str = concat!(
        "export function map(values) {\n",
        "  const keep = (item) => item;\n",
        "  return [\n",
        "    values.map(function (item) { return keep(item); }),\n",
        "    values.map(item => item),\n",
        "  ];\n",
        "}\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("checked callback source");
    let found = analysis.publication();
    found.validate_facts().expect("valid callback publication");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.mjs")
        .expect("callback unit");
    let mut identities = BTreeSet::new();
    for (decl, decl_offset, read, read_offset, owner) in [
        ("keep = (item)", 8, "=> item;", 3, "/map/keep"),
        ("function (item)", 10, "keep(item)", 5, "/map"),
        ("item => item)", 0, "item => item)", 8, "/map"),
    ] {
        let name_start = SOURCE.find(decl).expect("parameter declaration") + decl_offset;
        let read_start = SOURCE.find(read).expect("parameter read") + read_offset;
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.unit == unit.unit && row.range == module_runtime_range(name_start, "item")
            })
            .expect("physical callback parameter");
        let identity = &declaration.symbol;
        assert!(identities.insert(identity));
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(identity)
                && object.kinds.contains(&SymbolKind::Parameter)
                && object.name == "item"
        }));
        assert!(
            found.edges.iter().any(|edge| {
                edge.from.as_str().ends_with(owner)
                    && edge.to == *identity
                    && edge.kinds == [EdgeKind::Reads]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == unit.unit
                            && site.range == module_runtime_range(read_start, "item")
                    })
            }),
            "{read}"
        );
    }
}

#[test]
fn callback_parameters_preserve_earlier_package_declarations() {
    const SOURCE: &str =
        "export function map(values) { const keep = (item) => item; return keep(values); }";
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owned package");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let file = ProjectPath::new("index.mjs").expect("entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let sources = [PackageSource::new(&file, SOURCE)];
    let manifests = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
    )];
    let collect = |bound| {
        let limits = ExactPackageLimits::new(3, 4096)
            .with_declarations(bound)
            .expect("bound");
        let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
            .expect("checked source")
            .with_framework_context(&manifests, &[])
            .expect("package manifest");
        PackageAnalyzer::analyze(input, 1)
            .expect("publication")
            .into_parts()
            .0
    };
    let bounded = collect(4);
    let roomy = collect(8);
    assert_eq!(bounded.declarations.len(), 4);
    assert!(bounded.declarations.iter().any(|row| row.source == SOURCE));
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.source == "values")
    );
    assert!(roomy.declarations.iter().any(|row| row.source == "item"));
    assert!(
        bounded
            .declarations
            .iter()
            .all(|row| roomy.declarations.contains(row))
    );
    assert!(bounded.edges.iter().all(|edge| roomy.edges.contains(edge)));
    assert!(bounded.declarations.iter().all(|row| row.source != "item"));
}

#[test]
fn npm_callback_parameter_keeps_a_same_named_module_export() {
    let source = concat!(
        "export function run(values) { return values.map(item => item); }\n",
        "const item = 7;\n",
        "export { item as stable };\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("checked alias beside a callback");
    let found = analysis.publication();
    assert!(
        found.objects.iter().any(|object| {
            object.name == "item" && object.kinds.contains(&SymbolKind::Parameter)
        })
    );
    let module = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon";
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.mjs")
        .expect("module");
    let export = "export { item as stable };";
    let start = source.find(export).expect("written export");
    assert!(found.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{module}/stable")
            && edge.to.as_str() == format!("{module}/item")
            && edge.kinds == [EdgeKind::Aliases]
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.unit == unit.unit && site.range == module_runtime_range(start, export)
            })
    }));
}
