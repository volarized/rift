//! Publication of edges from checked file modules.

use super::*;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::read::{EdgeDerivation, EdgeProvider, SourceLocation};

#[test]
fn typescript_plain_annotations_use_the_checked_type_facet_of_a_namespace_merge() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#;
    const INTERFACE: &str = "export interface Mode { readonly state: string; }\n";
    const NAMESPACE: &str = "export declare namespace Mode { const Ready: number; }\n";
    const USE: &str = "export interface Consumer { check(mode: Mode): Mode; }\n";
    fn collect(source: &str, leaf: &str) -> PackageAnalysis {
        internal_modules::analyze(MANIFEST, &[("index.d.ts", source), ("leaf.d.ts", leaf)])
    }
    for (declaration, namespace_first) in [
        (INTERFACE, false),
        (INTERFACE, true),
        (
            "export declare class Mode { readonly state: string; }\n",
            false,
        ),
        ("export declare enum Mode { Off = 0 }\n", false),
        ("export type Mode = { readonly state: string };\n", false),
    ] {
        let merged = if namespace_first {
            format!("{NAMESPACE}{declaration}")
        } else {
            format!("{declaration}{NAMESPACE}")
        };
        for (source, path) in [
            (format!("{merged}{USE}"), "index.d.ts"),
            (
                format!("import {{ Mode }} from './leaf';\n{USE}"),
                "leaf.d.ts",
            ),
            (
                format!("import type {{ Mode }} from './leaf';\n{USE}"),
                "leaf.d.ts",
            ),
        ] {
            let found = collect(&source, &merged);
            let publication = found.publication();
            let written_unit = publication
                .units
                .iter()
                .find(|unit| unit.path.0 == "index.d.ts")
                .expect("original annotation unit");
            let target_unit = publication
                .units
                .iter()
                .find(|unit| unit.path.0 == path)
                .expect("original type unit");
            let declared = if path == "index.d.ts" {
                &source
            } else {
                &merged
            };
            let name_offset = declared.find(declaration).expect("written type item")
                + declaration.find("Mode").expect("written type name");
            let name = module_runtime_range(name_offset, "Mode");
            let type_declaration = publication
                .declarations
                .iter()
                .find(|row| {
                    row.unit == target_unit.unit
                        && row.symbol.as_str().ends_with("/Mode")
                        && row.range.start <= name.start
                        && name.end <= row.range.end
                })
                .expect("checked type declaration");
            let annotations = source.find(USE).expect("consumer declaration");
            let expected = [
                USE.find("Mode").expect("parameter"),
                USE.rfind("Mode").expect("return"),
            ]
            .map(|offset| module_runtime_range(annotations + offset, "Mode"));
            let annotation_count = publication
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds == [EdgeKind::HasType]
                        && edge.to == type_declaration.symbol
                        && edge.occurrence.as_ref().is_some_and(|site| {
                            site.unit == written_unit.unit && expected.contains(&site.range)
                        })
                })
                .count();
            assert_eq!(annotation_count, 2, "{source}");
        }
    }

    let unresolved = format!("import type {{ Mode }} from './missing';\n{USE}");
    let competing =
        format!("{INTERFACE}export interface Mode {{ other: number; }}\n{NAMESPACE}{USE}");
    for source in [unresolved, competing] {
        let found = collect(&source, "");
        let start = u64::try_from(source.find(USE).expect("consumer declaration"))
            .expect("bounded fixture offset");
        assert!(found.publication().edges.iter().all(|edge| {
            edge.kinds != [EdgeKind::HasType]
                || edge
                    .occurrence
                    .as_ref()
                    .is_none_or(|site| site.range.start < start)
        }));
    }
}

#[test]
fn checked_commonjs_export_object_values_keep_their_literal_imports() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.cjs"}"#;
    const SOURCE: &str = concat!(
        "const stable = require('./stable.cjs');\n",
        "module.exports = { part: require( './leaf.cjs' ), stable };\n"
    );
    let collect = |source| {
        internal_modules::analyze(
            MANIFEST,
            &[
                ("index.cjs", source),
                (
                    "leaf.cjs",
                    "exports.convert = function convert(value) { return value + 1; };",
                ),
                (
                    "stable.cjs",
                    "exports.stable = function stable(value) { return value + 1; };",
                ),
            ],
        )
    };
    let supported = collect(SOURCE);
    let publication = supported.publication();
    let source_module = publication
        .declarations
        .iter()
        .find(|row| row.source_complete && row.source == SOURCE)
        .expect("complete module source owner");

    let edges = publication
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Imports]);
    for specifier in ["'./leaf.cjs'", "'./stable.cjs'"] {
        let offset = SOURCE.find(specifier).expect("written specifier");
        assert!(
            edges.clone().any(|edge| {
                let Some(occurrence) = &edge.occurrence else {
                    return false;
                };
                occurrence.range == module_runtime_range(offset, specifier)
                    && edge.from == source_module.symbol
                    && publication
                        .units
                        .iter()
                        .any(|unit| unit.unit == occurrence.unit && unit.path.0 == "index.cjs")
                    && edge
                        .to
                        .as_str()
                        .ends_with(specifier.trim_matches('\'').trim_start_matches("./"))
            }),
            "missing import at {specifier}"
        );
    }
    for source in [
        "module.exports = { part: require('./missing.cjs') };",
        "const specifier = './leaf.cjs'; module.exports = { part: require(specifier) };",
        "const require = () => ({}); module.exports = { part: require('./leaf.cjs') };",
        "module.exports = { part: require('./leaf.cjs', sideEffect()) };",
        "module.exports = make({ part: require('./leaf.cjs') });",
    ] {
        let unsupported = collect(source);
        assert!(
            unsupported.publication().edges.iter().all(|edge| {
                !edge.kinds.contains(&EdgeKind::Imports) || !edge.to.as_str().ends_with("leaf.cjs")
            }),
            "unproved import in {source}"
        );
    }
}

#[test]
fn typescript_selectors_reference_uniquely_imported_declarations() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#;
    const LEAF: &str =
        "export interface Source { mode: string; }\nexport declare function prepare(): Source;\n";
    const SOURCE: &str = concat!(
        "import { Source as Local, prepare } from './leaf';\n",
        "export { Local as Public, prepare };\n",
        "export type { Local as Typed };\n"
    );
    let collect = |source| {
        internal_modules::analyze(MANIFEST, &[("index.d.ts", source), ("leaf.d.ts", LEAF)])
    };
    let found = collect(SOURCE);
    let publication = found.publication();
    let source = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.d.ts")
        .expect("original unit");
    let complete_owner = publication
        .declarations
        .iter()
        .find(|row| row.unit == source.unit && row.source_complete && row.source == SOURCE)
        .expect("complete checked type entry");
    let selectors = [
        (
            SOURCE.find("Local as Public").expect("renamed selector"),
            "Local",
            "/Source",
        ),
        (
            SOURCE.find("prepare };").expect("value selector"),
            "prepare",
            "/prepare",
        ),
        (
            SOURCE.find("Local as Typed").expect("type selector"),
            "Local",
            "/Source",
        ),
    ];
    for (offset, name, target) in selectors {
        let range = module_runtime_range(offset, name);
        assert!(
            publication
                .edges
                .iter()
                .any(|edge| edge.kinds == [EdgeKind::References]
                    && edge.from == complete_owner.symbol
                    && edge.to.as_str().ends_with(target)
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|occurrence| occurrence.unit == source.unit
                            && occurrence.range == range)),
            "missing imported selector {name}"
        );
    }
    for (offset, name) in [
        (SOURCE.find("Local").expect("written import"), "Local"),
        (SOURCE.find("prepare").expect("named import"), "prepare"),
    ] {
        let range = module_runtime_range(offset, name);
        assert!(publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Imports]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == source.unit && site.range == range)
        }));
    }
    assert!(
        publication
            .edges
            .iter()
            .any(|edge| edge.kinds == [EdgeKind::Aliases]
                && edge.from.as_str().ends_with("/Public")
                && edge.to.as_str().ends_with("/Source"))
    );
    for source in [
        "import { Source as Local } from './missing'; export { Local as Public };",
        "import * as Local from './leaf'; export { Local as Public };",
    ] {
        let unresolved = collect(source);
        assert!(
            unresolved
                .publication()
                .edges
                .iter()
                .all(|edge| edge.kinds != [EdgeKind::References])
        );
    }
}

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
fn typescript_local_export_selectors_reference_their_written_declarations() {
    const SOURCE: &str = concat!(
        "interface Inline { value: string }\n",
        "interface Renamed { value: number }\n",
        "interface Statement { value: boolean }\n",
        "interface Implicit { value: bigint }\n",
        "declare function value(): number;\n",
        "declare function defaultValue(): number;\n",
        "export { type Inline, type Renamed as PublicRenamed, value };\n",
        "export type { Statement };\n",
        "export { Implicit, defaultValue };\n",
        "export interface Direct { marker: string }\n",
        "declare const clash: string; declare const clash: number;\n",
        "export { clash, Missing };\n",
        "export { External } from './other.js';\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.d.ts", SOURCE),
            ("other.d.ts", "export interface External { marker: string }"),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#,
        )),
    )
    .expect("checked declaration package");
    let publication = analysis.publication();
    publication.validate_facts().expect("valid package facts");
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.d.ts")
        .expect("written declaration file")
        .unit;
    let module = publication
        .objects
        .iter()
        .find(|symbol| symbol.name == "beacon" && symbol.kinds.contains(&SymbolKind::Module))
        .and_then(|symbol| symbol.id.as_ref())
        .expect("checked entry module");
    let site = |statement: &str, name: &str| {
        let start = SOURCE.find(statement).expect("written statement");
        module_runtime_range(start + statement.find(name).expect("written token"), name)
    };
    for (statement, name) in [
        ("type Inline", "Inline"),
        ("type Renamed", "Renamed"),
        ("export type { Statement", "Statement"),
        ("Implicit, defaultValue", "Implicit"),
        ("Implicit, defaultValue", "defaultValue"),
        ("value };", "value"),
    ] {
        let range = site(statement, name);
        let target = format!("{}/{name}", module.as_str());
        assert_eq!(
            publication
                .edges
                .iter()
                .filter(|edge| {
                    &edge.from == module
                        && edge.to.as_str() == target
                        && edge.kinds == [EdgeKind::References]
                        && edge.occurrence.as_ref().is_some_and(|occurrence| {
                            &occurrence.unit == unit && occurrence.range == range
                        })
                })
                .count(),
            1,
            "checked local selector {name}"
        );
    }
    for (statement, name) in [
        ("as PublicRenamed", "PublicRenamed"),
        ("export interface Direct", "Direct"),
        ("export { clash, Missing", "clash"),
        ("export { clash, Missing", "Missing"),
        ("export { External", "External"),
    ] {
        let range = site(statement, name);
        assert!(!publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::References]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|occurrence| &occurrence.unit == unit && occurrence.range == range)
        }));
    }
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/PublicRenamed")
            && edge.to.as_str().ends_with("/Renamed")
            && edge.kinds == [EdgeKind::Aliases]
    }));
}

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
  return first ? { opaque: 5 } : [{ opaque: 6 }];
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
        .filter(|edge| edge.from.as_str() == MODULE && edge.kinds == [EdgeKind::Imports])
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
fn typescript_local_export_references_follow_existing_package_edges() {
    let (publication, files, semantics, _) = analyze_module(
        concat!(
            "type Shape = string; export { Shape as PublicShape };\n",
            "export function leaf() { return 1; }\n",
            "function schedule(task: () => number) { return task(); }\n",
            "schedule(() => leaf());\n",
        ),
        MANIFEST,
        PackageConfiguration::default(),
    )
    .into_parts();
    let selector = |edge: &rift_protocol::index::StoredEdge| {
        edge.from.as_str() == MODULE && edge.kinds == [EdgeKind::References]
    };
    let reference = publication
        .edges
        .iter()
        .find(|edge| selector(edge))
        .expect("one written selector");
    let callbacks = anonymous_callback_ids(&publication);
    assert_eq!(callbacks.len(), 1);
    let call = publication
        .edges
        .iter()
        .find(|edge| callbacks.contains(&edge.from) && edge.kinds == [EdgeKind::Calls])
        .expect("one checked callback call");
    let prior = publication
        .edges
        .iter()
        .filter(|edge| !selector(edge) && !callbacks.contains(&edge.from))
        .cloned()
        .collect::<Vec<_>>();
    assert!(prior.iter().any(|edge| edge.kinds == [EdgeKind::Aliases]));
    assert_eq!(publication.edges.len(), prior.len() + 2);
    for extra in 0..=2 {
        let mut records = Records {
            objects: publication
                .objects
                .iter()
                .filter(|row| row.id.as_ref().is_none_or(|id| !callbacks.contains(id)))
                .cloned()
                .collect(),
            declarations: publication
                .declarations
                .iter()
                .filter(|row| !callbacks.contains(&row.symbol))
                .cloned()
                .collect(),
            units: publication.units.clone(),
            edges: prior.clone(),
            ..Records::default()
        };
        let limits = ExactPackageLimits::new(3, 16_384)
            .with_edges(prior.len() + extra)
            .expect("bounded edge count");
        records
            .retain_ecmascript_modules(
                &publication.owner,
                &files,
                &semantics,
                limits,
                &BTreeSet::new(),
            )
            .expect("bounded module references");
        assert_eq!(records.edges.len(), prior.len() + extra);
        assert_eq!(&records.edges[..prior.len()], &prior);
        assert_eq!(records.edges.get(prior.len()), (extra >= 1).then_some(call));
        assert_eq!(
            records.edges.get(prior.len() + 1),
            (extra >= 2).then_some(reference)
        );
    }
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
    let callbacks = anonymous_callback_ids(&publication);
    let available_before = publication
        .edges
        .iter()
        .filter(|edge| !module_references.contains(edge))
        .filter(|edge| !callbacks.contains(&edge.from))
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

fn anonymous_callback_ids(publication: &PackagePublication) -> BTreeSet<&SymbolId> {
    publication
        .objects
        .iter()
        .filter(|row| row.document_local && row.name.starts_with("$callback#"))
        .filter_map(|row| row.id.as_ref())
        .collect()
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
fn secondary_javascript_argument_keys_keep_the_separate_written_module_binding() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./entry.js","module":"./entry.mjs","exports":{"import":{"default":"./entry.mjs"},"require":{"default":"./entry.js"}}}"#;
    const CLASS: &str = r"
var Channel = class {
  run(flag) {
    let current = 1;
    if (flag) { let current = 2; void (current + 4); }
    void (current + 3);
    return STATES.closed;
  }
};
";
    for (label, registration, after, expected) in [
        (
            "direct arrow",
            "register(api, { Channel: () => Channel });",
            "",
            true,
        ),
        (
            "direct primitive",
            "register(api, { Channel: 1 });",
            "",
            true,
        ),
        (
            "array position",
            "register(api, [null, { Channel: 1 }, null]);",
            "",
            false,
        ),
        (
            "duplicate keys",
            "register(api, { Channel: 1, Channel: 2 });",
            "",
            false,
        ),
        (
            "write to binding",
            "register(api, { Channel: 1 });",
            "Channel = replacement;",
            false,
        ),
        (
            "unknown scope",
            "if (enabled) { register(api, { Channel: 1 }); }",
            "",
            false,
        ),
    ] {
        let source = format!(
            "const api = {{}};\nfunction register() {{}}\nfunction expose(value) {{ return value; }}\n{registration}\nmodule.exports = expose(api);\nconst STATES = {{ closed: 1 }};\n{CLASS}\n{after}\n"
        );
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("entry.js", &source),
                ("entry.mjs", "export function anchor() { return 1; }"),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("both configured package routes");
        let publication = analysis.publication();
        let binding = publication
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with("/beacon%2Fentry.js/Channel"));
        let method = publication.declarations.iter().find(|row| {
            row.symbol
                .as_str()
                .ends_with("/beacon%2Fentry.js/Channel/run")
        });
        let paired = publication.declarations.iter().find(|row| {
            row.symbol.as_str().contains("/entry.js/Channel%23")
                || row.symbol.as_str().contains("%2Fentry.js/Channel%23")
        });
        assert_eq!(binding.is_some(), expected, "{label}");
        assert_eq!(method.is_some(), expected, "{label}");
        assert_eq!(paired.is_some(), expected, "{label}");
        if let (Some(binding), Some(method), Some(paired)) = (binding, method, paired) {
            assert_eq!(
                binding.syntax_element_kind.as_deref(),
                Some("variable_declarator")
            );
            assert_eq!(paired.syntax_element_kind.as_deref(), Some("pair"));
            assert_eq!(binding.unit, paired.unit);
            assert!(paired.range.end < binding.range.start, "{label}");
            assert_argument_collision_preserves_local_uses(publication, &source, method, label);
        }
    }
}

/// The supplemental class does not change the original checked lexical target of a use.
fn assert_argument_collision_preserves_local_uses(
    publication: &PackagePublication,
    source: &str,
    method: &PackageSymbol,
    label: &str,
) {
    let mut reads = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.from == method.symbol
                && edge.kinds == [EdgeKind::Reads]
                && edge.to.as_str().contains("/STATES")
        })
        .map(|edge| edge.to.as_str().rsplit('/').next().expect("written target"))
        .collect::<Vec<_>>();
    reads.sort_unstable();
    assert_eq!(reads, ["STATES", "closed"], "{label}");
    let locals = publication
        .declarations
        .iter()
        .filter(|row| row.symbol.as_str().ends_with("/current"))
        .collect::<Vec<_>>();
    assert_eq!(locals.len(), 2, "{label}");
    for (initializer, usage) in [
        ("current = 1", "current + 3"),
        ("current = 2", "current + 4"),
    ] {
        let item = module_runtime_range(
            source.find(initializer).expect("written local"),
            initializer,
        );
        let use_site = module_runtime_range(source.find(usage).expect("written use"), "current");
        let local = locals
            .iter()
            .find(|row| row.range == item && row.source_complete && row.source == initializer)
            .expect("local declaration");
        assert!(local.symbol.as_str().contains("/$scope%23"), "{label}");
        assert!(
            publication.occurrences.iter().any(|row| {
                row.symbol == local.symbol && row.unit == local.unit && row.range == use_site
            }),
            "{label}: {initializer}"
        );
    }
}

#[test]
fn direct_function_body_arguments_keep_sibling_classes_and_written_members() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","exports":{".":{"import":"./entry.mjs","require":"./entry.js"}}}"#;
    const CLASS: &str = "var Channel = class {
  run(flag) {
    let current = 1;
    if (flag) { let current = 2; void (current + 4); }
    void (current + 3);
    return STATES.closed;
  }
};";
    for (label, start, end) in [
        ("arrow initializer", "var Bundle = (() => {", "})();"),
        (
            "function initializer",
            "var Bundle = (function () {",
            "})();",
        ),
        ("named function", "function Bundle() {", "}"),
    ] {
        for (statement, expected) in [
            ("register(api, { Channel: () => Channel });", true),
            (
                "if (enabled) { register(api, { Channel: () => Channel }); }",
                false,
            ),
            ("register(api, [{ Channel: () => Channel }]);", false),
        ] {
            let source = format!(
                "{start}
const api = {{}};
function register() {{}}
{statement}
const STATES = {{ closed: 1 }};
{CLASS}
return api;
{end}
module.exports = Bundle;
"
            );
            let result = fixture::package_result_with_metadata(
                ShippedLanguage::JavaScript,
                vec![
                    ("entry.js", &source),
                    ("entry.mjs", "export function anchor() { return 1; }"),
                ],
                None,
                Some(("package.json", MANIFEST)),
            )
            .expect("both checked entries");
            let publication = result.publication();
            let declarations = &publication.declarations;
            let item = module_runtime_range(
                source.find(CLASS).expect("written class") + 4,
                &CLASS[4..CLASS.len() - 1],
            );
            let binding = declarations.iter().find(|row| {
                row.range == item
                    && row.source_complete
                    && row.symbol.as_str().ends_with("/Channel")
            });
            let property = "Channel: () => Channel";
            let site = module_runtime_range(
                source.find(property).expect("written argument property"),
                property,
            );
            let paired = declarations.iter().find(|row| {
                row.range == site
                    && row.source_complete
                    && row.symbol.as_str().contains("/Channel%23")
            });
            assert_eq!(binding.is_some(), expected, "{label}: {statement}");
            assert_eq!(paired.is_some(), expected, "{label}: {statement}");
            if let (Some(binding), Some(paired)) = (binding, paired) {
                assert_eq!(binding.unit, paired.unit);
                let method = declarations
                    .iter()
                    .find(|row| {
                        row.unit == binding.unit && row.symbol.as_str().ends_with("/Channel/run")
                    })
                    .expect("the restored class owns its written method");
                assert_argument_collision_preserves_local_uses(publication, &source, method, label);
            }
        }
    }
}

#[test]
fn checked_named_function_initializers_keep_independent_argument_properties() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":"./main.mjs"}}"#;
    for (holder, open, close, count) in [
        ("function named(input)", "", "", 2),
        ("function (input)", "", "", 0),
        (
            "function named(input)",
            "return remote.combine(function (item) {",
            "});",
            2,
        ),
        (
            "function named(input)",
            "const other = function (item) {",
            "}; return other;",
            0,
        ),
    ] {
        let source = format!(
            "import * as remote from 'external';
function emit(value) {{ return value; }}
export const run = {holder} {{
  {open}
  return emit([
    remote.copy(input, {{ key: 1, first: true }}),
    remote.copy(input, {{ key: 2, second: false }}),
  ]);
  {close}
}};"
        );
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("main.mjs", "export const entry = true;"),
                ("case.mjs", &source),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked package");
        let publication = analysis.publication();
        let owner = publication
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with("/case.mjs/run")
                    || row.symbol.as_str().ends_with("%2Fcase.mjs/run")
            })
            .expect("the complete variable declaration remains");
        let mut repeated = BTreeSet::new();
        for spelling in ["key: 1", "key: 2", "first: true", "second: false"] {
            let site =
                module_runtime_range(source.find(spelling).expect("written property"), spelling);
            let found = publication
                .declarations
                .iter()
                .find(|row| row.unit == owner.unit && row.source == spelling && row.range == site);
            if spelling.starts_with("key:") {
                if let Some(property) = found {
                    assert!(property.symbol.as_str().starts_with(owner.symbol.as_str()));
                    repeated.insert(property.symbol.as_str());
                }
            } else {
                assert!(found.is_some(), "incumbent property {spelling}");
            }
        }
        assert_eq!(repeated.len(), count, "{holder}");
    }
}

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
fn non_entry_literal_properties_keep_uniquely_named_descendants() {
    const SOURCE: &str = concat!(
        "function receive(value) { return value; }\n",
        "receive({ details: { first: { score: 1 }, shared: 2 } });\n",
        "receive({ details: { second: { score: 3 }, shared: 4 } });\n",
        "const ordinary = { details: { existing: 5 } };\n",
        "export const marker = ordinary;\n",
    );
    let analyzed = non_entry_lexical_binding_analysis(SOURCE, 96);
    let graph = analyzed.publication();
    graph.validate_facts().expect("checked owner paths");
    let declaration = |written: &str| {
        let start = SOURCE.find(written).expect("written member");
        graph
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("physical property")
    };
    let mut owners = BTreeSet::new();
    for (details, child, leaf, kept) in [
        (
            "details: { first: { score: 1 }, shared: 2 }",
            "first: { score: 1 }",
            "score: 1",
            "shared: 2",
        ),
        (
            "details: { second: { score: 3 }, shared: 4 }",
            "second: { score: 3 }",
            "score: 3",
            "shared: 4",
        ),
    ] {
        let [details, child, leaf, kept] = [details, child, leaf, kept].map(declaration);
        assert!(owners.insert(&details.symbol));
        for (parent, member) in [(details, child), (details, kept), (child, leaf)] {
            assert_eq!(parent.unit, member.unit);
            assert!(graph.objects.iter().any(|row| {
                row.id.as_ref() == Some(&member.symbol)
                    && row.container.as_ref() == Some(&parent.symbol)
            }));
        }
    }
    assert!(
        declaration("existing: 5")
            .symbol
            .as_str()
            .ends_with("/ordinary/details/existing")
    );
    let limited = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(limited.publication().declarations, graph.declarations);
    assert!(
        limited
            .publication()
            .edges
            .iter()
            .all(|edge| graph.edges.contains(edge))
    );
}

#[test]
fn object_elements_of_a_written_call_array_keep_distinct_property_declarations() {
    const SOURCE: &str = concat!(
        "function keep(label, value) { return value; }\n",
        "export function build(key) {\n",
        "  keep('slots', [{ tag: 'first' }, { tag: 'second' }]);\n",
        "  const rows = [{ tag: 'third' }, { tag: 'fourth' }];\n",
        "  keep('control', rows);\n",
        "  keep('computed', [{ [key]: 'other' }]);\n",
        "}\n",
    );
    let analyzed = non_entry_lexical_binding_analysis(SOURCE, 128);
    let graph = analyzed.publication();
    graph.validate_facts().expect("written call array");
    let mut seen = BTreeSet::new();
    for value in ["first", "second", "third", "fourth"] {
        let written = format!("tag: '{value}'");
        let start = SOURCE.find(&written).expect("authored array slot");
        let declaration = graph
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, &written))
            .expect("checked property declaration");
        assert!(seen.insert(&declaration.symbol));
    }
    let computed = "[key]: 'other'";
    let computed_start = SOURCE.find(computed).expect("written computed property");
    let mut computed_rows = graph
        .declarations
        .iter()
        .filter(|row| row.source == computed);
    let retained = computed_rows.next().expect("existing computed property");
    assert!(computed_rows.next().is_none());
    assert!(retained.symbol.as_str().ends_with("/build/%5Bkey%5D"));
    assert_eq!(
        retained.range,
        module_runtime_range(computed_start, computed)
    );
    let call = "keep('slots', [{ tag: 'first' }, { tag: 'second' }])";
    let start = SOURCE.find(call).expect("written array call");
    assert!(graph.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/build")
            && edge.to.as_str().ends_with("/keep")
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.range == module_runtime_range(start, call)
                    && site.unit.0.ends_with("/reader.mjs")
            })
    }));
    let capped = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(capped.publication().declarations, graph.declarations);
    assert!(
        capped
            .publication()
            .edges
            .iter()
            .all(|edge| graph.edges.contains(edge))
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

#[test]
fn npm_typescript_overloads_retain_their_physical_parameters() {
    const SOURCE: &str = concat!(
        "export function convert(value: string): string;\n",
        "export function convert(value: number): number;\n",
        "export function convert(value: string | number) { return value; }\n",
        "export interface Store {\n",
        "  read(key: string): string;\n",
        "  read(index: number): number;\n",
        "}\n",
        "export class Table {\n",
        "  read(record: string): string;\n",
        "  read(record: number): number;\n",
        "  read(record: string | number) { return record; }\n",
        "}\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.ts"}}}"#,
        )),
    )
    .expect("checked overload source");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("valid overloaded declarations");
    let mut identities = BTreeSet::new();
    for written in [
        "value: string)",
        "value: number)",
        "value: string | number)",
        "key: string)",
        "index: number)",
        "record: string)",
        "record: number)",
        "record: string | number)",
    ] {
        let start = SOURCE.find(written).expect("parameter source");
        let name = written.split(':').next().expect("parameter name");
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.unit.as_str().ends_with("/index.ts")
                    && offset_in(row.range.start) == start
                    && SOURCE
                        .get(start..offset_in(row.range.end))
                        .is_some_and(|source| source.starts_with(name))
            })
            .expect("physical overload parameter");
        assert!(identities.insert(&declaration.symbol));
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(&declaration.symbol)
                && object.kinds.contains(&SymbolKind::Parameter)
                && object.name == name
        }));
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        if written.contains('|') {
            let [read] = reads.as_slice() else {
                panic!("implementation read of {name}")
            };
            let source = read.occurrence.as_ref().expect("authored reference");
            let start = SOURCE
                .find(&format!("return {name}"))
                .expect("returned parameter")
                + 7;
            assert_eq!(source.unit, declaration.unit);
            assert_eq!(source.range, module_runtime_range(start, name));
        } else {
            assert!(reads.is_empty(), "{written} has no implementation");
        }
    }
}

#[test]
fn overload_parameters_preserve_regular_parameter_and_module_slots() {
    const SOURCE: &str = concat!(
        "export function choose(value: string): string;\n",
        "export function choose(value: number): number;\n",
        "export function choose(value: string | number) { return value; }\n",
        "export function plain(ordinary: number) { return ordinary; }\n",
    );
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owned package");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::TypeScript.language();
    let file = ProjectPath::new("index.ts").expect("entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let sources = [PackageSource::new(&file, SOURCE)];
    let manifests = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.ts"}}}"#,
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
    let bounded = collect(6);
    let roomy = collect(10);
    assert_eq!(bounded.declarations.len(), 6);
    assert!(bounded.declarations.iter().any(|row| row.source == SOURCE));
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/plain/ordinary"))
    );
    assert!(
        bounded
            .declarations
            .iter()
            .all(|row| roomy.declarations.contains(row))
    );
    assert!(bounded.edges.iter().all(|edge| roomy.edges.contains(edge)));
    assert_eq!(roomy.declarations.len() - bounded.declarations.len(), 3);
}

#[test]
fn npm_javascript_methods_retain_their_value_parameters_and_reads() {
    const SOURCE: &str = concat!(
        "export const table = {\n",
        "  read(key) { return key; },\n",
        "};\n",
        "export class Reader {\n",
        "  read(key) { const reroute = (key) => key; return reroute(key); }\n",
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
    .expect("checked method source");
    let found = analysis.publication();
    found.validate_facts().expect("valid package facts");
    for (holder, declaration_start, reading, offset) in [
        (
            "table",
            SOURCE.find("read(key)").expect("object method") + 5,
            "return key",
            7,
        ),
        (
            "Reader",
            SOURCE.rfind("read(key)").expect("class method") + 5,
            "reroute(key);",
            8,
        ),
    ] {
        let suffix = format!("/{holder}/read/key");
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(declaration_start, "key")
            })
            .expect("physical method parameter");
        let owner = format!("/{holder}/read");
        let read_start = SOURCE.find(reading).expect("authored use") + offset;
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read of the {holder} method parameter")
        };
        assert!(read.from.as_str().ends_with(&owner));
        let occurrence = read.occurrence.as_ref().expect("physical read");
        assert_eq!(occurrence.unit, declaration.unit);
        assert_eq!(occurrence.range, module_runtime_range(read_start, "key"));
    }
}

#[test]
fn javascript_method_parameters_preserve_existing_parameter_slots() {
    const SOURCE: &str = concat!(
        "export const table = { read(key) { return key; } };\n",
        "export function plain(initial) { const keep = (item) => item; return keep(initial); }\n",
    );
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
    let bounded = collect(7);
    let roomy = collect(9);
    assert_eq!(bounded.declarations.len(), 7);
    assert!(bounded.declarations.iter().any(|row| row.source == SOURCE));
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/plain/initial"))
    );
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/keep/item"))
    );
    assert!(
        bounded
            .declarations
            .iter()
            .all(|row| roomy.declarations.contains(row))
    );
    assert!(bounded.edges.iter().all(|edge| roomy.edges.contains(edge)));
    let added = roomy
        .declarations
        .iter()
        .filter(|row| !bounded.declarations.contains(row))
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 1);
    assert!(added[0].symbol.as_str().ends_with("/table/read/key"));
}

#[test]
fn standalone_typescript_signatures_keep_their_written_value_parameters() {
    const SOURCE: &str = concat!(
        "export declare function display(value: string, radix?: number): string;\n",
        "export function plain(input: string) { return input; }\n",
    );
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owned package");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::TypeScript.language();
    let file = ProjectPath::new("index.ts").expect("entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let sources = [PackageSource::new(&file, SOURCE)];
    let manifests = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.ts"}}}"#,
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
    roomy
        .validate_facts()
        .expect("valid signature declarations");
    assert_eq!(bounded.declarations.len(), 4);
    assert!(bounded.declarations.iter().any(|row| row.source == SOURCE));
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/plain/input"))
    );
    assert!(
        bounded
            .declarations
            .iter()
            .all(|row| roomy.declarations.contains(row))
    );
    assert!(bounded.edges.iter().all(|edge| roomy.edges.contains(edge)));
    assert_eq!(roomy.declarations.len() - bounded.declarations.len(), 2);
    for name in ["value", "radix"] {
        let suffix = format!("/display/{name}");
        let declaration = roomy
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with(&suffix))
            .expect("authored signature parameter");
        let start = SOURCE.find(name).expect("parameter declaration");
        assert_eq!(declaration.range, module_runtime_range(start, name));
        assert!(
            !roomy
                .edges
                .iter()
                .any(|edge| { edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads] })
        );
    }
}

#[test]
fn javascript_scoped_function_parameters_use_their_established_owner() {
    const SOURCE: &str = concat!(
        "const values = [];\n",
        "{ function visit(key) { return key; } values.push(visit(1)); }\n",
        "{ function visit(key) { return key; } values.push(visit(2)); }\n",
        "export { values };\n",
    );
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
    let found = collect(8);
    assert_eq!(bounded.declarations.len(), 4);
    assert_eq!(found.declarations.len() - bounded.declarations.len(), 2);
    assert!(bounded.declarations.iter().any(|row| row.source == SOURCE));
    assert!(
        bounded
            .declarations
            .iter()
            .all(|row| found.declarations.contains(row))
    );
    assert!(bounded.edges.iter().all(|edge| found.edges.contains(edge)));
    found.validate_facts().expect("physical scoped parameters");
    let names = SOURCE
        .match_indices("visit(key)")
        .map(|(start, _)| start + 6);
    let reads = SOURCE
        .match_indices("return key")
        .map(|(start, _)| start + 7);
    for ((declaration_start, read_start), owner) in names.zip(reads).zip(["visit", "visit%231"]) {
        let suffix = format!("/{owner}/key");
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(declaration_start, "key")
            })
            .expect("written scoped parameter");
        let parameter_reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = parameter_reads.as_slice() else {
            panic!("one read in {owner}")
        };
        assert!(read.from.as_str().ends_with(&format!("/{owner}")));
        let site = read.occurrence.as_ref().expect("written read");
        assert_eq!(site.unit, declaration.unit);
        assert_eq!(site.range, module_runtime_range(read_start, "key"));
    }
}

#[test]
fn non_entry_javascript_lexical_bindings_keep_their_physical_scope() {
    const SOURCE: &str = concat!(
        "export function choose(flag) {\n",
        "  if (flag) { const result = 1; return result; }\n",
        "  { const result = 2; return result; }\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("valid physical declarations and reads");
    let held = analysis
        .files()
        .iter()
        .find(|file| file.file().path().as_str() == "reader.mjs")
        .expect("retained module");
    assert!(held.placement().logical_identity("choose.result").is_none());
    let unit = &found
        .units
        .iter()
        .find(|row| row.path.0 == "reader.mjs")
        .expect("physical unit")
        .unit;
    let bindings = ["result = 1", "result = 2"].map(|spelling| {
        let start = SOURCE.find(spelling).expect("variable declaration");
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                &row.unit == unit
                    && row.range == module_runtime_range(start, spelling)
                    && row.source == spelling
            })
            .expect("physical lexical declaration");
        assert!(!declaration.public);
        let read_start = SOURCE[start..]
            .find("return result")
            .expect("authored read")
            + start
            + 7;
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read for {spelling}");
        };
        let site = read.occurrence.as_ref().expect("physical read");
        assert_eq!(site.unit, declaration.unit);
        assert_eq!(site.range, module_runtime_range(read_start, "result"));
        declaration.symbol.clone()
    });
    assert_ne!(bindings[0], bindings[1]);

    let primary = found
        .edges
        .iter()
        .filter(|edge| !bindings.contains(&edge.to))
        .cloned()
        .collect::<Vec<_>>();
    assert!(!primary.is_empty());
    // A module-level name cannot be treated as a second private lexical scope.
    let module_scope = concat!(
        "const external = 8;\n",
        "run(() => { const external = 1; return external; });\n",
    );
    let without_override = non_entry_lexical_binding_analysis(module_scope, 64);
    let rows = &without_override.publication().declarations;
    assert!(rows.iter().all(|row| row.source != "external = 1"));

    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    let bounded = bounded.publication();
    assert!(bounded.edges.iter().all(|edge| primary.contains(edge)));
    assert_eq!(bounded.declarations, found.declarations);
}

fn non_entry_lexical_binding_analysis(source: &str, edge_cap: usize) -> PackageAnalysis {
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("owned package");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let entry = ProjectPath::new("index.mjs").expect("entry");
    let reader = ProjectPath::new("reader.mjs").expect("non-entry");
    let manifest = ProjectPath::new("package.json").expect("manifest");
    let files = [
        PackageSource::new(&entry, "export const ready = true;"),
        PackageSource::new(&reader, source),
    ];
    let context = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
    )];
    let limits = ExactPackageLimits::new(3, 4096)
        .with_edges(edge_cap)
        .expect("edge cap");
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
        .expect("checked source files")
        .with_framework_context(&context, &[])
        .expect("npm manifest");
    PackageAnalyzer::analyze(input, 1).expect("validated publication")
}

#[test]
fn typescript_structured_declarations_own_their_written_type_parameters() {
    const SOURCE: &str = concat!(
        "export interface Reader<Value> { get(): Value; }\n",
        "export type Identity<Value> = (value: Value) => Value;\n",
        "export class Box<Value> { read(value: Value): Value { return value; } }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked generic declarations");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("valid authored type bindings");
    for (owner, expected) in [("Reader", 1), ("Identity", 2), ("Box", 2)] {
        let suffix = format!("/{owner}/Value");
        let start = SOURCE
            .find(&format!("{owner}<Value>"))
            .expect("generic owner")
            + owner.len()
            + 1;
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, "Value")
                    && row.source == "Value"
            })
            .expect("written type parameter");
        assert!(found.objects.iter().any(|row| {
            row.id.as_ref() == Some(&declaration.symbol)
                && row
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::TypeParameter)
        }));
        let end = SOURCE[start..].find('\n').expect("generic declaration end") + start;
        let ranges = SOURCE[start + 5..end]
            .match_indices("Value")
            .map(|(offset, _)| {
                let range = module_runtime_range(start + 5 + offset, "Value");
                (range.start, range.end)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(ranges.len(), expected);
        let uses = found
            .edges
            .iter()
            .filter(|edge| edge.to == declaration.symbol && edge.kinds == [EdgeKind::HasType])
            .map(|edge| {
                let site = edge.occurrence.as_ref().expect("written type use");
                assert_eq!(site.unit, declaration.unit);
                (site.range.start, site.range.end)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(uses, ranges);
    }
}

#[test]
fn javascript_shorthand_property_preserves_both_the_key_and_parameter() {
    const SOURCE: &str = concat!(
        "export function choose(value) { return { value }; }\n",
        "export function control(input) { return { output: input }; }\n",
    );
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
            .expect("validated source")
            .with_framework_context(&manifests, &[])
            .expect("package manifest");
        PackageAnalyzer::analyze(input, 1)
            .expect("validated publication")
            .into_parts()
            .0
    };
    let found = collect(10);
    found
        .validate_facts()
        .expect("valid property and parameter declarations");
    let written = SOURCE.find("choose(value)").expect("signature") + "choose(".len();
    let shorthand = SOURCE.find("{ value }").expect("property") + "{ ".len();
    let property = found
        .declarations
        .iter()
        .find(|row| {
            row.symbol.as_str().ends_with("/choose/value")
                && row.range == module_runtime_range(shorthand, "value")
        })
        .expect("existing shorthand property identity");
    let parameter = found
        .declarations
        .iter()
        .find(|row| row.range == module_runtime_range(written, "value") && row.source == "value")
        .expect("authored value parameter");
    assert_ne!(parameter.symbol, property.symbol);
    assert_eq!(parameter.unit, property.unit);
    assert!(found.objects.iter().any(|row| {
        row.id.as_ref() == Some(&parameter.symbol)
            && row
                .kinds
                .contains(&rift_protocol::read::SymbolKind::Parameter)
    }));
    let reads = found
        .edges
        .iter()
        .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::Reads])
        .collect::<Vec<_>>();
    let [read] = reads.as_slice() else {
        panic!("one authored shorthand Read");
    };
    assert_eq!(
        read.occurrence.as_ref().expect("physical Read").range,
        property.range
    );
    let bounded = collect(6);
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/control/input"))
    );
    assert!(
        bounded
            .declarations
            .iter()
            .any(|row| row.symbol == property.symbol)
    );
    assert!(
        !bounded
            .declarations
            .iter()
            .any(|row| row.symbol == parameter.symbol)
    );
    assert!(bounded.edges.iter().all(|edge| found.edges.contains(edge)));
}

#[test]
fn typescript_callable_type_signatures_keep_distinct_parameter_ranges() {
    const SOURCE: &str = concat!(
        "export type Reader = (value: string) => number;\n",
        "export interface Service { (value: string): Reader; new (value: number): Service; }\n",
        "export function run(initial: string) { return initial; }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked callable type scopes");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical callable type parameters");
    let mut identities = BTreeSet::new();
    for (position, _) in SOURCE.match_indices("value:") {
        let declaration = found
            .declarations
            .iter()
            .find(|row| {
                row.range == module_runtime_range(position, "value") && row.source == "value"
            })
            .expect("written callable type parameter");
        assert!(found.objects.iter().any(|row| {
            row.id.as_ref() == Some(&declaration.symbol)
                && row
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::Parameter)
        }));
        assert!(identities.insert(&declaration.symbol));
        assert!(
            !found
                .edges
                .iter()
                .any(|edge| { edge.to == declaration.symbol && edge.kinds == [EdgeKind::Reads] })
        );
    }
    assert_eq!(identities.len(), 3);
    assert!(found.edges.iter().any(|edge| {
        edge.to.as_str().ends_with("/run/initial") && edge.kinds == [EdgeKind::Reads]
    }));
}

#[test]
fn typescript_index_signature_parameters_keep_separate_non_runtime_scopes() {
    const SOURCE: &str = concat!(
        "export interface Inventory { readonly [key: string]: number; [key: number]: number; }\n",
        "export type Table = { readonly [name: string]: Inventory };\n",
        "export function read(key: string) { return key; }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked signature scopes");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical index signature parameters");
    let mut identities = BTreeSet::new();
    for (signature, name, owner) in [
        ("[key: string]", "key", "Inventory"),
        ("[key: number]", "key", "Inventory"),
        ("[name: string]", "name", "Table"),
    ] {
        let position = SOURCE.find(signature).expect("written signature") + 1;
        let parameter = found
            .declarations
            .iter()
            .find(|row| row.range == module_runtime_range(position, name) && row.source == name)
            .expect("written index signature parameter");
        assert!(parameter.symbol.as_str().contains(&format!("/{owner}/")));
        assert!(identities.insert(&parameter.symbol));
        assert!(
            !found
                .edges
                .iter()
                .any(|edge| { edge.to == parameter.symbol && edge.kinds == [EdgeKind::Reads] })
        );
    }
    assert_eq!(identities.len(), 3);
    assert!(found.edges.iter().any(|edge| {
        edge.to.as_str().ends_with("/read/key") && edge.kinds == [EdgeKind::Reads]
    }));
}

#[test]
fn javascript_defaulted_and_rest_parameters_use_their_written_body_scope() {
    const SOURCE: &str = concat!(
        "const seed = 1;\n",
        "export function withDefault(value = seed) { return value; }\n",
        "export function withRest(...items) { return items; }\n",
        "export function control(input) { return input; }\n",
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
    .expect("checked wrapped parameters");
    let found = analysis.publication();
    found.validate_facts().expect("physical parameter bindings");
    for (owner, name, header) in [
        ("withDefault", "value", "value = seed"),
        ("withRest", "items", "items)"),
        ("control", "input", "input)"),
    ] {
        let suffix = format!("/{owner}/{name}");
        let start = SOURCE.find(header).expect("physical parameter");
        let parameter = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, name)
                    && row.source == name
            })
            .expect("written parameter");
        let reading = format!("return {name}");
        let at = SOURCE.find(&reading).expect("body reference") + "return ".len();
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read for {owner}");
        };
        assert!(read.from.as_str().ends_with(&format!("/{owner}")));
        let written = read.occurrence.as_ref().expect("physical parameter read");
        assert_eq!(written.range, module_runtime_range(at, name));
        assert_eq!(written.unit, parameter.unit);
    }
    let seed = SOURCE.find("= seed").expect("written initializer") + "= ".len();
    assert!(found.edges.iter().any(|edge| {
        edge.to.as_str().ends_with("/seed")
            && edge.kinds == [EdgeKind::Reads]
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.range == module_runtime_range(seed, "seed"))
    }));
}

#[test]
fn javascript_object_and_array_parameters_keep_their_written_lexical_bindings() {
    const SOURCE: &str = concat!(
        "const key = 1;\n",
        "export function select({ key }, [{ label: item }]) { return [key, item]; }\n",
        "export function outside() { return key; }\n",
        "export function ordinary(input) { return input; }\n",
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
    .expect("checked destructured parameters");
    let found = analysis.publication();
    found.validate_facts().expect("physical parameter facts");
    for (name, header, marker) in [
        ("key", "{ key }", "[key, item]"),
        ("item", "item }", "item];"),
    ] {
        let start = SOURCE.find(header).expect("written binding") + usize::from(name == "key") * 2;
        let suffix = format!("/select/{name}");
        let parameter = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, name)
                    && row.source == name
            })
            .expect("authored pattern binding");
        assert!(found.objects.iter().any(|row| {
            row.id.as_ref() == Some(&parameter.symbol)
                && row
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::Parameter)
        }));
        let read_start = SOURCE.find(marker).expect("written read") + usize::from(name == "key");
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read of {name}");
        };
        assert!(read.from.as_str().ends_with("/select"));
        let written = read.occurrence.as_ref().expect("physical read");
        assert_eq!(written.unit, parameter.unit);
        assert_eq!(written.range, module_runtime_range(read_start, name));
    }
    assert!(found.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/outside")
            && edge.to.as_str().ends_with("/beacon/key")
            && edge.kinds == [EdgeKind::Reads]
    }));
    assert!(found.edges.iter().any(|edge| {
        edge.to.as_str().ends_with("/ordinary/input") && edge.kinds == [EdgeKind::Reads]
    }));
}

#[test]
fn non_entry_typescript_union_properties_keep_their_written_branches() {
    const SOURCE: &str = concat!(
        "export type Result = { kind: 'ready'; value: number } | { kind: 'waiting'; value: string };\n",
        "export function read(result: Result) { return result.kind; }\n",
    );
    let analyze = |entry| {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","types":"./{entry}"}}"#);
        fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.ts", "export const ready = true;"),
                ("result.ts", SOURCE),
            ],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("checked union sources")
    };
    let non_entry = analyze("index.ts");
    let entry = analyze("result.ts");
    let found = non_entry.publication();
    found.validate_facts().expect("physical union members");
    let entry_facts = entry.publication();
    let branch = |publication: &PackagePublication, property: &str| {
        let start = SOURCE.find(property).expect("written property");
        publication
            .declarations
            .iter()
            .find(|row| {
                row.range == module_runtime_range(start, property) && row.source == property
            })
            .expect("physical union branch property")
            .symbol
            .clone()
    };
    for pair in [
        ["kind: 'ready'", "kind: 'waiting'"],
        ["value: number", "value: string"],
    ] {
        let separate = pair.map(|property| branch(found, property));
        assert_ne!(separate[0], separate[1]);
        assert_eq!(branch(entry_facts, pair[0]), branch(entry_facts, pair[1]));
        assert!(
            found
                .edges
                .iter()
                .all(|edge| !separate.contains(&edge.to) || edge.kinds != [EdgeKind::Reads])
        );
    }
    assert!(
        found
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/Result"))
    );
}

#[test]
fn non_entry_typescript_wrapped_and_return_unions_keep_their_written_branches() {
    const SOURCE: &str = concat!(
        "export type Direct = { prior: number } | { prior: string };\n",
        "export type Wrapped = Promise<{ value: number } | { value: string }>;\n",
        "export declare class Reader {\n",
        "    plain(): { code: number } | { code: string };\n",
        "    wrapped(): Promise<{ rendered: number } | { rendered: string }>;\n",
        "}\n",
        "export declare function read(): Promise<{ result: number } | { result: string }>;\n",
        "export declare function accept(input: { other: number } | { other: string }): void;\n",
    );
    let analyze = |entry| {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","types":"./{entry}"}}"#);
        fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                (
                    "index.d.ts",
                    "export { Direct, Reader, Wrapped, read } from './reader.js';",
                ),
                ("reader.d.ts", SOURCE),
            ],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("written type sources")
    };
    let non_entry = analyze("index.d.ts");
    let entry = analyze("reader.d.ts");
    let found = non_entry.publication();
    found.validate_facts().expect("checked branch owners");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "reader.d.ts")
        .expect("written declaration file");
    for (owner, name) in [
        ("Direct", "prior"),
        ("Wrapped", "value"),
        ("Reader/plain", "code"),
        ("Reader/wrapped", "rendered"),
        ("read", "result"),
    ] {
        let mut identities = BTreeSet::new();
        for ty in ["number", "string"] {
            let written = format!("{name}: {ty}");
            let start = SOURCE.find(&written).expect("written property");
            let range = module_runtime_range(start, &written);
            let declaration = found
                .declarations
                .iter()
                .find(|row| row.unit == unit.unit && row.range == range && row.source == written)
                .unwrap_or_else(|| panic!("missing {owner}/{written}"));
            let suffix = format!("%2Freader.d.ts/{owner}/{name}");
            assert!(declaration.symbol.as_str().contains(&suffix));
            assert!(identities.insert(&declaration.symbol));
            assert!(entry.publication().declarations.iter().any(|row| {
                row.unit == unit.unit
                    && row.range == range
                    && row.source == written
                    && row.symbol.as_str().ends_with(&format!("/{owner}/{name}"))
            }));
        }
    }
    assert!(
        found
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/accept"))
    );
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.symbol.as_str().contains("/accept/other"))
    );
}

#[test]
fn typescript_method_type_parameters_keep_their_own_annotation_targets() {
    const SOURCE: &str = concat!(
        "export interface Reader { read<Value>(value: Value): Value; }\n",
        "export class Box { read<Value>(value: Value): Value { return value; } }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked methods");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical method type parameters");
    for owner in ["Reader", "Box"] {
        let holder = SOURCE.find(owner).expect("written owner");
        let method = SOURCE[holder..]
            .find("read<Value>")
            .expect("written method")
            + holder;
        let start = method + "read<".len();
        let suffix = format!("/{owner}/read/Value");
        let parameter = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, "Value")
                    && row.source == "Value"
            })
            .expect("physical method type parameter");
        let signature = "read<Value>(value: Value): Value";
        let expected = signature
            .match_indices("Value")
            .skip(1)
            .map(|(offset, _)| {
                let range = module_runtime_range(method + offset, "Value");
                (range.start, range.end)
            })
            .collect::<BTreeSet<_>>();
        let actual = found
            .edges
            .iter()
            .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::HasType])
            .map(|edge| {
                let occurrence = edge.occurrence.as_ref().expect("physical type use");
                assert_eq!(occurrence.unit, parameter.unit);
                (occurrence.range.start, occurrence.range.end)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
    assert!(found.edges.iter().any(|edge| {
        edge.to.as_str().ends_with("/Box/read/value") && edge.kinds == [EdgeKind::Reads]
    }));
}

#[test]
fn non_entry_typescript_union_properties_keep_scopes_across_written_alternatives() {
    const SOURCE: &str = concat!(
        "export type Result =\n",
        "  | { tag: 'first'; value: number }\n",
        "  | { tag: 'waiting'; message: string }\n",
        "  | { tag: 'last'; value: string };\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.ts", "export const ready = true;"),
            ("result.ts", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked alias");
    let found = analysis.publication();
    found.validate_facts().expect("physical union members");
    for properties in [
        &["tag: 'first'", "tag: 'waiting'", "tag: 'last'"][..],
        &["value: number", "value: string"][..],
    ] {
        let mut identities = BTreeSet::new();
        for property in properties {
            let start = SOURCE.find(property).expect("written branch property");
            let declared = found
                .declarations
                .iter()
                .find(|row| {
                    row.range == module_runtime_range(start, property) && row.source == *property
                })
                .expect("physical branch declaration");
            assert!(identities.insert(&declared.symbol));
        }
    }
}

#[test]
fn typescript_named_function_type_parameters_link_only_their_own_types() {
    const SOURCE: &str = concat!(
        "export function choose<Value>(parameter: Value): Value { return parameter; }\n",
        "export function echo<Value>(parameter: Value): Value { return parameter; }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked generic functions");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical function type parameters");
    for owner in ["choose", "echo"] {
        let signature = format!("{owner}<Value>(parameter: Value): Value");
        let method = SOURCE.find(&signature).expect("written function");
        let start = method + owner.len() + 1;
        let suffix = format!("/{owner}/Value");
        let parameter = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, "Value")
                    && row.source == "Value"
            })
            .expect("physical function type parameter");
        let expected = signature
            .match_indices("Value")
            .skip(1)
            .map(|(offset, _)| {
                let range = module_runtime_range(method + offset, "Value");
                (range.start, range.end)
            })
            .collect::<BTreeSet<_>>();
        let actual = found
            .edges
            .iter()
            .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::HasType])
            .map(|edge| {
                let occurrence = edge.occurrence.as_ref().expect("physical type use");
                assert_eq!(occurrence.unit, parameter.unit);
                (occurrence.range.start, occurrence.range.end)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
}

#[test]
fn non_entry_javascript_patterns_keep_their_written_lexical_bindings() {
    const SOURCE: &str = concat!(
        "export function choose(flag, source) {\n",
        "  if (flag) { const { value } = source; return value; }\n",
        "  { const [value] = source; return value; }\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical local pattern facts");
    let bindings =
        [("{ value }", 2), ("[value]", 1)].map(|(header, offset)| {
            let position = SOURCE.find(header).expect("written pattern") + offset;
            let binding = found
                .declarations
                .iter()
                .find(|row| {
                    row.range == module_runtime_range(position, "value") && row.source == "value"
                })
                .expect("written local pattern binding");
            assert!(!binding.public);
            assert!(found.objects.iter().any(|row| {
                row.id.as_ref() == Some(&binding.symbol) && row.kind.0 == "variable"
            }));
            let read_position = SOURCE[position..]
                .find("return value")
                .expect("written read")
                + position
                + "return ".len();
            let reads = found
                .edges
                .iter()
                .filter(|edge| edge.to == binding.symbol && edge.kinds == [EdgeKind::Reads])
                .collect::<Vec<_>>();
            let [read] = reads.as_slice() else {
                panic!("one read for each locally bound name");
            };
            let occurrence = read.occurrence.as_ref().expect("physical read");
            assert_eq!(occurrence.unit, binding.unit);
            assert_eq!(
                occurrence.range,
                module_runtime_range(read_position, "value")
            );
            binding.symbol.clone()
        });
    assert_ne!(bindings[0], bindings[1]);
}

#[test]
fn non_entry_javascript_member_calls_keep_separate_argument_properties() {
    const SOURCE: &str = concat!(
        "const receiver = { read(options) { return options.status; } };\n",
        "export function choose() {\n",
        "  receiver.read({ status: 'first' });\n",
        "  receiver.read({ status: 'second' });\n",
        "  const named = { status: 'stable' };\n",
        "  return receiver.read(named);\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical argument properties");
    let mut identities = BTreeSet::new();
    for property in ["status: 'first'", "status: 'second'", "status: 'stable'"] {
        let start = SOURCE.find(property).expect("written key");
        let declared = found
            .declarations
            .iter()
            .find(|row| {
                row.range == module_runtime_range(start, property) && row.source == property
            })
            .expect("physical member argument property");
        assert!(identities.insert(&declared.symbol));
        if property == "status: 'stable'" {
            assert!(declared.symbol.as_str().ends_with("/choose/named/status"));
        }
    }
    let calls = found
        .edges
        .iter()
        .filter(|edge| {
            edge.from.as_str().ends_with("/choose")
                && edge.to.as_str().ends_with("/receiver/read")
                && edge.kinds == [EdgeKind::Calls]
        })
        .map(|edge| edge.occurrence.as_ref().expect("written call").range.start)
        .collect::<BTreeSet<_>>();
    let written = SOURCE
        .match_indices("receiver.read(")
        .map(|(start, _)| u64::try_from(start).expect("physical call position"))
        .collect::<BTreeSet<_>>();
    assert_eq!(calls, written);
}

#[test]
fn non_entry_class_methods_keep_properties_of_returned_call_arguments() {
    const SOURCE: &str = concat!(
        "export class Builder {\n",
        "  convert(first) {\n",
        "    if (first) return this.wrap({ state: 'initial', value: 1 });\n",
        "    return this.wrap({ state: 'final', value: 2 });\n",
        "  }\n",
        "  wrap(data) { return data; }\n",
        "  existing() {\n",
        "    const options = { stable: 3 };\n",
        "    return this.wrap(options);\n",
        "  }\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical argument properties");

    let mut identities = BTreeSet::new();
    for written in ["state: 'initial'", "value: 1", "state: 'final'", "value: 2"] {
        let start = SOURCE.find(written).expect("written property");
        let property = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("class method argument property");
        assert!(!property.public);
        assert!(property.symbol.as_str().contains("/Builder/convert/"));
        assert!(identities.insert(&property.symbol));
    }
    assert!(found.declarations.iter().any(|row| {
        row.source == "stable: 3"
            && row
                .symbol
                .as_str()
                .ends_with("/Builder/existing/options/stable")
    }));
    let calls = found
        .edges
        .iter()
        .filter(|edge| {
            edge.from.as_str().ends_with("/Builder/convert")
                && edge.to.as_str().ends_with("/Builder/wrap")
                && edge.kinds == [EdgeKind::Calls]
        })
        .map(|edge| {
            let range = &edge.occurrence.as_ref().expect("written call").range;
            (range.start, range.end)
        })
        .collect::<BTreeSet<_>>();
    let written = [
        "this.wrap({ state: 'initial', value: 1 })",
        "this.wrap({ state: 'final', value: 2 })",
    ]
    .map(|call| {
        let range = module_runtime_range(SOURCE.find(call).expect("authored call"), call);
        (range.start, range.end)
    });
    assert_eq!(calls, written.into_iter().collect());

    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn non_entry_named_class_methods_keep_properties_in_separate_return_literals() {
    const SOURCE: &str = concat!(
        "function selectName() { return 'runtime'; }\n",
        "export class Builder {\n",
        "  convert(first) {\n",
        "    if (first) return { state: 'first' };\n",
        "    return { state: 'last' };\n",
        "  }\n",
        "  static prepare(first) {\n",
        "    if (first) return { ready: 1 };\n",
        "    return { ready: 2 };\n",
        "  }\n",
        "  [selectName()](first) {\n",
        "    if (first) return { unknown: 3 };\n",
        "    return { unknown: 4 };\n",
        "  }\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("authored method return properties");
    for (method, properties) in [
        ("convert", ["state: 'first'", "state: 'last'"]),
        ("prepare", ["ready: 1", "ready: 2"]),
    ] {
        let suffix = format!("/Builder/{method}");
        let owner = found
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with(&suffix))
            .expect("named class method");
        let rows = properties.map(|written| {
            let start = SOURCE.find(written).expect("written property");
            let row = found
                .declarations
                .iter()
                .find(|row| {
                    row.unit == owner.unit
                        && row.source == written
                        && row.range == module_runtime_range(start, written)
                })
                .expect("physical method return property");
            assert!(!row.public);
            assert!(found.objects.iter().any(|object| {
                object.id.as_ref() == Some(&row.symbol)
                    && object.container.as_ref() == Some(&owner.symbol)
            }));
            &row.symbol
        });
        assert_ne!(rows[0], rows[1]);
    }
    assert!(
        found
            .declarations
            .iter()
            .any(|row| { row.source.starts_with("[selectName()](first)") })
    );
    assert!(
        found
            .declarations
            .iter()
            .all(|row| { !matches!(row.source.as_str(), "unknown: 3" | "unknown: 4") })
    );
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn non_entry_conditional_returns_keep_distinct_literal_branch_properties() {
    const SOURCE: &str = concat!(
        "export function build(first) {\n",
        "  return first ? ({ state: 1, ready: true }) : ({ state: 2, ready: false });\n",
        "}\n",
        "export function direct(first) {\n",
        "  if (first) return { stable: 3 };\n",
        "  return { stable: 4 };\n",
        "}\n",
        "export function caller(first) { const result = build(first); return result.state; }\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("authored conditional branch properties");

    let mut identities = BTreeSet::new();
    for (owner, written) in [
        ("build", "state: 1"),
        ("build", "ready: true"),
        ("build", "state: 2"),
        ("build", "ready: false"),
        ("direct", "stable: 3"),
        ("direct", "stable: 4"),
    ] {
        let start = SOURCE.find(written).expect("written property");
        let property = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("placed property");
        assert!(!property.public);
        assert!(property.symbol.as_str().contains(&format!("/{owner}/")));
        assert!(identities.insert(&property.symbol));
    }
    assert!(found.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/caller")
            && edge.to.as_str().ends_with("/build")
            && edge.kinds == [EdgeKind::Calls]
    }));
    assert!(
        found
            .edges
            .iter()
            .all(|edge| { edge.kinds != [EdgeKind::Reads] || !identities.contains(&edge.to) })
    );
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn package_javascript_callback_patterns_keep_separate_reads() {
    const SOURCE: &str = concat!(
        "export function project(records) {\n",
        "  const fromTuple = ([value]) => value;\n",
        "  const left = records.map(({value}) => value);\n",
        "  return [left, fromTuple([4]), records.map(({value}) => value + 1)];\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 24);
    let publication = analysis.publication();
    publication
        .validate_facts()
        .expect("authored callback patterns");
    let range = |start| module_runtime_range(start, "value");
    let mut symbols = BTreeSet::new();
    for (start, _) in SOURCE
        .match_indices("value])")
        .chain(SOURCE.match_indices("value})"))
    {
        let binding = publication
            .declarations
            .iter()
            .find(|row| row.source == "value" && row.range == range(start))
            .expect("physical callback binding");
        assert!(!binding.public);
        assert!(symbols.insert(&binding.symbol));
        let read = SOURCE[start..]
            .find("=> value")
            .expect("written callback read")
            + start
            + 3;
        let references = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to == binding.symbol)
            .collect::<Vec<_>>();
        let [reference] = references.as_slice() else {
            panic!("one read to each callback binding");
        };
        let at = reference
            .occurrence
            .as_ref()
            .expect("written read location");
        assert_eq!(at.range, range(read));
        assert_eq!(at.unit, binding.unit);
    }
    assert_eq!(symbols.len(), 3);
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/project")
            && edge.to.as_str().ends_with("/project/fromTuple")
            && edge.kinds == [EdgeKind::Calls]
    }));
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, publication.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| { publication.edges.contains(edge) && !symbols.contains(&edge.to) })
    );
}

#[test]
fn non_entry_returns_keep_nested_keys_under_their_physical_parent() {
    const SOURCE: &str = concat!(
        "export function build(first) {\n",
        "  if (first) return { payload: { code: 1 } };\n",
        "  return { payload: { code: 2 } };\n",
        "}\n",
        "export function duplicate(first) {\n",
        "  if (first) return { opaque: { key: 3, key: 4 } };\n",
        "  return { opaque: { key: 5, key: 6 } };\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found.validate_facts().expect("authored nested properties");

    let property = |written: &str| {
        let start = SOURCE.find(written).expect("written property");
        found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("placed property")
    };
    let ids = [
        ("payload: { code: 1 }", "code: 1"),
        ("payload: { code: 2 }", "code: 2"),
    ]
    .map(|(outer, inner)| {
        let parent = property(outer);
        let member = property(inner);
        assert_eq!(member.unit, parent.unit);
        assert!(!member.public);
        assert!(found.objects.iter().any(|row| {
            row.id.as_ref() == Some(&member.symbol)
                && row.container.as_ref() == Some(&parent.symbol)
        }));
        member.symbol.clone()
    });
    assert_ne!(ids[0], ids[1]);
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.source.starts_with("key: "))
    );
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn non_entry_callbacks_keep_properties_in_distinct_call_arguments() {
    const SOURCE: &str = concat!(
        "function inspect(value) { return value.status; }\n",
        "export function choose(items) {\n",
        "  return items.map(item => {\n",
        "    if (item) return inspect({ status: 'first' });\n",
        "    return inspect({ status: 'second' });\n",
        "  });\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("callback argument properties");
    let properties = ["status: 'first'", "status: 'second'"].map(|written| {
        let start = SOURCE.find(written).expect("written property");
        let row = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("placed property");
        assert!(!row.public);
        row.symbol.clone()
    });
    assert_ne!(properties[0], properties[1]);
    let calls = found
        .edges
        .iter()
        .filter(|row| row.kinds == [EdgeKind::Calls] && row.to.as_str().ends_with("/inspect"))
        .map(|row| row.occurrence.as_ref().expect("written call").range.start)
        .collect::<BTreeSet<_>>();
    let written = SOURCE
        .match_indices("return inspect(")
        .map(|(start, _)| u64::try_from(start + "return ".len()).expect("call position"))
        .collect::<BTreeSet<_>>();
    assert_eq!(calls, written);
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    let bounded = bounded.publication();
    let full_callbacks = anonymous_callback_ids(found);
    let bounded_callbacks = anonymous_callback_ids(bounded);
    assert_eq!(full_callbacks.len(), 1);
    assert!(bounded_callbacks.is_subset(&full_callbacks));
    assert_eq!(
        bounded
            .declarations
            .iter()
            .filter(|row| !bounded_callbacks.contains(&row.symbol))
            .collect::<Vec<_>>(),
        found
            .declarations
            .iter()
            .filter(|row| !full_callbacks.contains(&row.symbol))
            .collect::<Vec<_>>()
    );
    assert!(bounded_callbacks.iter().all(|callback| {
        bounded
            .edges
            .iter()
            .any(|edge| &edge.from == *callback && edge.kinds == [EdgeKind::Calls])
    }));
    assert!(bounded.edges.iter().all(|edge| found.edges.contains(edge)));
}

#[test]
fn admitted_anonymous_callbacks_keep_written_argument_properties() {
    const SOURCE: &str = concat!(
        "function take(options) { return options.state; }\n",
        "function schedule(callback) { return callback(); }\n",
        "export function stable() { return 0; }\n",
        "schedule(() => {\n",
        "  take({ state: 'first', uniqueFirst: true });\n",
        "  take({ state: 'second', uniqueSecond: false });\n",
        "  schedule(() => {\n",
        "    take({ state: 'third' });\n",
        "    take({ state: 'fourth' });\n",
        "    take({ duplicate: 1, duplicate: 2 });\n",
        "    unknown({ state: 'unknown' });\n",
        "  });\n",
        "});\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 128);
    let graph = analysis.publication();
    graph.validate_facts().expect("argument properties");
    let callers = anonymous_callback_ids(graph);
    assert_eq!(callers.len(), 2);
    let mut properties = BTreeSet::new();
    for value in ["first", "second", "third", "fourth"] {
        let written = format!("state: '{value}'");
        let start = SOURCE.find(&written).expect("written property");
        let property = graph
            .declarations
            .iter()
            .find(|row| row.range == module_runtime_range(start, &written) && row.source == written)
            .expect("placed argument property");
        assert!(property.source_complete && !property.public);
        assert!(properties.insert(property.symbol.clone()));
        let object = graph
            .objects
            .iter()
            .find(|object| object.id.as_ref() == Some(&property.symbol))
            .expect("property identity");
        assert_eq!(object.name, "state");
        let caller = object.container.as_ref().expect("argument property owner");
        assert!(callers.contains(caller));
        assert!(
            caller
                .as_str()
                .ends_with(if matches!(value, "first" | "second") {
                    "$callback%230"
                } else {
                    "$callback%231"
                })
        );
        assert!(graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from == *caller
                && edge.to.as_str().ends_with("/take")
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == property.unit
                        && site.range.start <= property.range.start
                        && property.range.end <= site.range.end
                })
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Contains]
                && edge.from == *caller
                && edge.to == property.symbol
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == property.unit && site.range == module_runtime_range(start, "state")
                })
        }));
    }
    assert!(
        graph
            .declarations
            .iter()
            .any(|row| row.source == "uniqueFirst: true")
    );
    assert!(
        graph
            .declarations
            .iter()
            .any(|row| row.source == "uniqueSecond: false")
    );
    assert!(graph.objects.iter().all(|object| {
        !object
            .container
            .as_ref()
            .is_some_and(|id| callers.contains(id))
            || object.name != "duplicate"
    }));
    assert!(
        !graph
            .declarations
            .iter()
            .any(|row| row.source == "state: 'unknown'")
    );
    let capped = non_entry_lexical_binding_analysis(SOURCE, 2);
    let capped = capped.publication();
    assert!(capped.edges.len() <= 2);
    assert!(capped.edges.iter().all(|edge| graph.edges.contains(edge)));
}

#[test]
fn scoped_call_initializers_keep_argument_properties_without_result_reads() {
    const SOURCE: &str = concat!(
        "function wrap(value) { return { other: 1 }; }\n",
        "export function build(first) {\n",
        "  if (first === 0) { let record; return record; }\n",
        "  if (first === 1) {\n",
        "    const record = wrap({ code: 'one' });\n",
        "    return record.code;\n",
        "  }\n",
        "  const record = wrap({ code: 'two' });\n",
        "  return record.code;\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found.validate_facts().expect("physical call arguments");
    let declaration = |written: &str| {
        let start = SOURCE.find(written).expect("written declaration");
        found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("physical declaration")
    };
    for (index, written) in ["code: 'one'", "code: 'two'"].into_iter().enumerate() {
        let call = format!("wrap({{ {written} }})");
        let local = declaration(&format!("record = {call}"));
        let property = declaration(written);
        assert!(
            local
                .symbol
                .as_str()
                .ends_with(&format!("/record%23{}", index + 1))
        );
        assert_eq!(
            property.symbol.as_str(),
            format!("{}/code", local.symbol.as_str())
        );
        assert_eq!(property.unit, local.unit);
        assert!(!property.public);
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(&property.symbol)
                && object.container.as_ref() == Some(&local.symbol)
                && object.name == "code"
        }));
        let start = SOURCE.find(&call).expect("written call");
        assert!(found.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from.as_str().ends_with("/build")
                && edge.to.as_str().ends_with("/wrap")
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == property.unit
                        && occurrence.range == module_runtime_range(start, &call)
                })
        }));
        assert!(
            found.edges.iter().all(|edge| {
                edge.to != property.symbol || !edge.kinds.contains(&EdgeKind::Reads)
            })
        );
    }
    assert!(
        declaration("record")
            .symbol
            .as_str()
            .ends_with("/record%230")
    );
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn non_entry_callable_initializers_keep_literal_argument_properties() {
    const SOURCE: &str = concat!(
        "function wrap(value) { return value; }\n",
        "export function build(first) {\n",
        "  const choose = (condition) => {\n",
        "    wrap(condition ? { state: 'opaque' } : null);\n",
        "    if (condition) return wrap({ state: 'one' });\n",
        "    return wrap({ state: 'two' });\n",
        "  };\n",
        "  return choose(first);\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("authored argument properties");
    let properties = ["state: 'one'", "state: 'two'"].map(|written| {
        let start = SOURCE.find(written).expect("written property");
        let row = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("placed callable argument property");
        assert!(row.symbol.as_str().contains("/build/choose/state"));
        assert!(!row.public);
        row.symbol.clone()
    });
    assert_ne!(properties[0], properties[1]);
    assert!(
        found
            .declarations
            .iter()
            .all(|row| row.source != "state: 'opaque'")
    );
    for (from, to, written) in [
        ("/choose", "/wrap", "wrap({ state: 'one' })"),
        ("/choose", "/wrap", "wrap({ state: 'two' })"),
        (
            "/choose",
            "/wrap",
            "wrap(condition ? { state: 'opaque' } : null)",
        ),
        ("/build", "/choose", "choose(first)"),
    ] {
        let start = SOURCE.rfind(written).expect("written call");
        assert!(found.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from.as_str().ends_with(from)
                && edge.to.as_str().ends_with(to)
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.range == module_runtime_range(start, written)
                })
        }));
    }
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn javascript_file_modules_keep_local_names_without_an_established_method() {
    const SOURCE: &str = concat!(
        "export const codec = flag\n",
        "  ? { decode(text) { const result = text; return result; } }\n",
        "  : { decode(text) { const result = String(text); return result; } };\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found.validate_facts().expect("physical private bindings");
    assert!(
        found
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/codec"))
    );
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.symbol.as_str().ends_with("/codec/decode"))
    );
    let mut names = BTreeSet::new();
    for written in ["result = text", "result = String(text)"] {
        let start = SOURCE.find(written).expect("written binding");
        let row = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("physical local under a checked file module");
        assert!(!row.public);
        let identity = rift_protocol::identity::SymbolIdentity::parse(row.symbol.as_str())
            .expect("checked identity");
        assert_eq!(
            identity.qualified_path().first().map(String::as_str),
            Some("beacon/reader.mjs")
        );
        assert!(
            identity
                .qualified_path()
                .get(1)
                .is_some_and(|scope| scope.starts_with("$scope#"))
        );
        assert_eq!(
            identity.qualified_path().last().map(String::as_str),
            Some("result")
        );
        assert!(names.insert(row.symbol.clone()));
        let position = SOURCE[start..].find("return result").expect("written read") + start + 7;
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == row.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read of the written variable");
        };
        let site = read.occurrence.as_ref().expect("physical read");
        assert_eq!(site.unit, row.unit);
        assert_eq!(site.range, module_runtime_range(position, "result"));
    }
    assert_eq!(names.len(), 2);
    let repeated = non_entry_lexical_binding_analysis(SOURCE, 64);
    let repeated_names = repeated
        .publication()
        .declarations
        .iter()
        .filter(|row| row.source.starts_with("result = "))
        .map(|row| row.symbol.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(repeated_names, names);
}

#[test]
fn typescript_typed_patterns_keep_bindings_apart_from_the_written_type() {
    const SOURCE: &str = "export function pick({ key: label }: { key: string }, [item]: [number]) { return [label, item]; }\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("checked typed bindings");
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("physical typed binding facts");
    let property = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/pick/key") && row.source == "key: string")
        .expect("existing annotation declaration");
    for name in ["label", "item"] {
        let start = SOURCE.find(name).expect("written pattern token");
        let suffix = format!("/pick/{name}");
        let parameter = found
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with(&suffix)
                    && row.range == module_runtime_range(start, name)
                    && row.source == name
            })
            .expect("written typed parameter binding");
        assert_ne!(parameter.symbol, property.symbol);
        assert!(found.objects.iter().any(|row| {
            row.id.as_ref() == Some(&parameter.symbol)
                && row
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::Parameter)
        }));
        let reads = found
            .edges
            .iter()
            .filter(|edge| edge.to == parameter.symbol && edge.kinds == [EdgeKind::Reads])
            .collect::<Vec<_>>();
        let [read] = reads.as_slice() else {
            panic!("one read of {name}");
        };
        assert!(read.from.as_str().ends_with("/pick"));
        let written = read.occurrence.as_ref().expect("physical read");
        assert_eq!(written.unit, parameter.unit);
        assert_eq!(
            written.range,
            module_runtime_range(SOURCE.rfind(name).expect("body reference"), name)
        );
    }
}

#[test]
fn returned_literals_keep_proved_properties_when_a_same_named_owner_is_opaque() {
    const SOURCE: &str = concat!(
        "function inspect(value) { return value; }\n",
        "export function build(first, second) {\n",
        "  if (first) return { status: 'initial' };\n",
        "  inspect(second ? { status: 'opaque' } : null);\n",
        "  return { status: 'last' };\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 48);
    let found = analysis.publication();
    found.validate_facts().expect("proved returned properties");
    let properties = ["status: 'initial'", "status: 'last'"].map(|written| {
        let start = SOURCE.find(written).expect("written property");
        let row = found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .expect("placed property");
        assert!(!row.public);
        assert!(row.symbol.as_str().contains("/build/status%23"));
        row.symbol.clone()
    });
    assert_ne!(properties[0], properties[1]);
    assert!(
        found
            .declarations
            .iter()
            .all(|row| row.source != "status: 'opaque'")
    );
    assert!(found.edges.iter().any(|row| {
        row.kinds == [EdgeKind::Calls]
            && row.from.as_str().ends_with("/build")
            && row.to.as_str().ends_with("/inspect")
    }));
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

const JAVASCRIPT_LEXICAL_ALIASES: &str = concat!(
    "function leaf(value) { return value; }\n",
    "function branch(value) { return value + 1; }\n",
    "export function run(value) {\n",
    "  const alias = leaf;\n",
    "  const nested = alias;\n",
    "  return alias(value) + nested(value) + branch(value);\n",
    "}\n",
    "export function reassigned(value) {\n",
    "  let alias = leaf; alias = branch; return alias(value);\n",
    "}\n",
    "export function shadowed(value) {\n",
    "  const alias = leaf;\n",
    "  return function deferred(alias) { return alias(value); };\n",
    "}\n",
    "export function choice(flag, value) {\n",
    "  const alias = flag ? leaf : branch; return alias(value);\n",
    "}\n",
    "export function unresolved(value) {\n",
    "  const alias = missing; return alias(value);\n",
    "}\n",
    "export function property(value) {\n",
    "  const alias = leaf; return alias.unknown(value);\n",
    "}\n",
    "export function replaced(value) {\n",
    "  function changing(input) { return input; }\n",
    "  const alias = changing; changing = branch; return alias(value);\n",
    "}\n",
);

#[test]
fn javascript_calls_follow_unchanged_local_function_aliases() {
    let analysis = non_entry_lexical_binding_analysis(JAVASCRIPT_LEXICAL_ALIASES, 96);
    let found = analysis.publication();
    found.validate_facts().expect("checked function aliases");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.source == JAVASCRIPT_LEXICAL_ALIASES)
        .expect("retained package module");
    let calls = found
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .map(|edge| {
            let occurrence = edge.occurrence.as_ref().expect("written invocation");
            assert_eq!(occurrence.unit, unit.unit);
            assert!(edge.from.as_str().ends_with("/run"));
            (
                edge.to.as_str().rsplit('/').next().expect("target name"),
                (occurrence.range.start, occurrence.range.end),
            )
        })
        .collect::<BTreeSet<_>>();
    let expected = [
        ("leaf", "alias(value)"),
        ("leaf", "nested(value)"),
        ("branch", "branch(value)"),
    ]
    .map(|(target, written)| {
        let from_body = JAVASCRIPT_LEXICAL_ALIASES
            .find("return alias(value)")
            .expect("written body");
        let start = JAVASCRIPT_LEXICAL_ALIASES[from_body..]
            .find(written)
            .expect("authored call");
        let range = module_runtime_range(from_body + start, written);
        (target, (range.start, range.end))
    })
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(calls, expected);
    for (written, target) in [("alias = leaf", "/leaf"), ("alias(value)", "/run/alias")] {
        let start = JAVASCRIPT_LEXICAL_ALIASES
            .find(written)
            .expect("written reference");
        let (offset, name) = if target == "/leaf" {
            (8, "leaf")
        } else {
            (0, "alias")
        };
        assert!(found.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to.as_str().ends_with(target)
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == unit.unit
                        && occurrence.range == module_runtime_range(start + offset, name)
                })
        }));
    }
    let incumbent = found
        .edges
        .iter()
        .filter(|edge| edge.kinds != [EdgeKind::Calls] || !edge.to.as_str().ends_with("/leaf"))
        .cloned()
        .collect::<Vec<_>>();
    let bounded = non_entry_lexical_binding_analysis(JAVASCRIPT_LEXICAL_ALIASES, incumbent.len());
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| incumbent.contains(edge))
    );
    assert!(
        bounded.publication().edges.iter().any(|edge| {
            edge.to.as_str().ends_with("/branch") && edge.kinds == [EdgeKind::Calls]
        })
    );
}

#[test]
fn typescript_alias_types_do_not_record_calls() {
    const SOURCE: &str = concat!(
        "function leaf() { return 1; }\n",
        "const callable = leaf;\n",
        "export type Shape = typeof callable;\n",
        "export function run() { return callable(); }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.ts"}"#,
        )),
    )
    .expect("typed package");
    let found = analysis.publication();
    let calls = found
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].from.as_str().ends_with("/run"));
    assert!(calls[0].to.as_str().ends_with("/leaf"));
    let start = SOURCE.find("callable()").expect("written invocation");
    let occurrence = calls[0].occurrence.as_ref().expect("written invocation");
    assert_eq!(occurrence.range, module_runtime_range(start, "callable()"));
}

const JAVASCRIPT_ASSIGNED_RESULTS: &str = concat!(
    "function leaf(value) { return value; }\n",
    "function other(value) { return value + 1; }\n",
    "function make() { return { leaf }; }\n",
    "function uncertain(flag) { if (flag) return { leaf }; return null; }\n",
    "export function direct(value) {\n",
    "  const receiver = make(); return receiver.leaf(value);\n",
    "}\n",
    "export function assigned(value, unrelated, key) {\n",
    "  let receiver; receiver = make(); unrelated[key] = 0;\n",
    "  return receiver.leaf(value);\n",
    "}\n",
    "export function before(value) {\n",
    "  let receiver; const old = receiver.leaf(value); receiver = make(); return old;\n",
    "}\n",
    "export function conditional(flag, value) {\n",
    "  let receiver; if (flag) receiver = make(); return receiver.leaf(value);\n",
    "}\n",
    "export function reassigned(value) {\n",
    "  let receiver; receiver = make(); receiver = null; return receiver.leaf(value);\n",
    "}\n",
    "export function changed(value) {\n",
    "  let receiver; receiver = make(); receiver.leaf = other; return receiver.leaf(value);\n",
    "}\n",
    "export function escaped(take, value) {\n",
    "  let receiver; receiver = make(); take(receiver); return receiver.leaf(value);\n",
    "}\n",
    "export function deferred(value) {\n",
    "  let receiver; receiver = make(); return () => receiver.leaf(value);\n",
    "}\n",
    "export function differentShape(flag, value) {\n",
    "  let receiver; receiver = uncertain(flag); return receiver.leaf(value);\n",
    "}\n",
);

#[test]
fn javascript_members_follow_a_single_checked_call_result_assignment() {
    let source = JAVASCRIPT_ASSIGNED_RESULTS;
    let analysis = non_entry_lexical_binding_analysis(source, 120);
    let found = analysis.publication();
    found.validate_facts().expect("checked assignment results");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.source == source)
        .expect("retained package module");
    let direct_start = source
        .find("receiver.leaf(value)")
        .expect("written direct use");
    let assigned = source
        .find("export function assigned")
        .expect("written assignment");
    let assigned_start = assigned
        + source[assigned..]
            .find("receiver.leaf(value)")
            .expect("written use after assignment");
    let mut new_edges = Vec::new();
    for (kind, text) in [
        (EdgeKind::Reads, "receiver.leaf"),
        (EdgeKind::Calls, "receiver.leaf(value)"),
    ] {
        let at = |start: usize| {
            found
                .edges
                .iter()
                .find(|edge| {
                    edge.kinds == [kind]
                        && edge.occurrence.as_ref().is_some_and(|occurrence| {
                            occurrence.unit == unit.unit
                                && occurrence.range == module_runtime_range(start, text)
                        })
                })
                .unwrap_or_else(|| panic!("written member edge {kind:?} at {start}"))
        };
        let direct = at(direct_start);
        let after = at(assigned_start);
        assert!(direct.from.as_str().ends_with("/direct"));
        assert!(after.from.as_str().ends_with("/assigned"));
        assert_eq!(direct.to, after.to);
        new_edges.push(after.clone());
    }
    assert_eq!(
        found
            .edges
            .iter()
            .filter(|edge| {
                edge.to == new_edges[0].to
                    && matches!(edge.kinds.as_slice(), [EdgeKind::Reads | EdgeKind::Calls])
                    && edge.occurrence.as_ref().is_some_and(|occurrence| {
                        occurrence.unit == unit.unit
                            && source
                                [usize::try_from(occurrence.range.start).expect("source offset")..]
                                .starts_with("receiver.leaf(")
                    })
            })
            .count(),
        4
    );
    let bounded = non_entry_lexical_binding_analysis(source, found.edges.len() - new_edges.len());
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| { found.edges.contains(edge) && !new_edges.contains(edge) })
    );
}

#[test]
fn javascript_nested_property_arrays_keep_separate_written_slots() {
    const SOURCE: &str = concat!(
        "export const rows = [\n",
        "  { direct: 1, expected: [{ field: 1 }, { field: 2 }] },\n",
        "  { control: 3 },\n",
        "];\n",
        "export const cases = [\n",
        "  { groups: [{ member: 3 }, { member: 4 }] },\n",
        "  { groups: [{ member: 5 }, { member: 6 }] },\n",
        "];\n",
        "const duplicates = [{ expected: [{ repeated: 7, repeated: 8 }] }];\n",
        "export function withheld(key) {\n",
        "  const values = [{ [key]: [{ opaque: 9 }, { opaque: 10 }] }];\n",
        "  return values;\n",
        "}\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", "export { rows } from './data.js';"),
            ("data.js", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#,
        )),
    )
    .expect("checked nested arrays");
    let found = analysis.publication();
    found.validate_facts().expect("valid property facts");
    let unit = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "data.js")
        .expect("nested array source")
        .unit;
    let mut identities = BTreeSet::new();
    let mut groups = BTreeSet::new();
    for (written, owner) in [
        ("field: 1", "expected"),
        ("field: 2", "expected"),
        ("member: 3", "groups"),
        ("member: 4", "groups"),
        ("member: 5", "groups"),
        ("member: 6", "groups"),
    ] {
        let start = SOURCE.find(written).expect("written property");
        let range = module_runtime_range(start, written);
        let row = found
            .declarations
            .iter()
            .find(|row| &row.unit == unit && row.range == range && row.source == written)
            .unwrap_or_else(|| panic!("missing written nested array property {written}"));
        let identity = SymbolIdentity::parse(row.symbol.as_str()).expect("property identity");
        let path = identity.qualified_path();
        let parent = &path[path.len() - 2];
        assert_eq!(parent.split('#').next(), Some(owner));
        if owner == "groups" {
            groups.insert(parent.clone());
        }
        assert!(identities.insert(row.symbol.clone()));
    }
    assert_eq!(groups.len(), 2);
    for retained in ["direct: 1", "control: 3"] {
        assert!(
            found
                .declarations
                .iter()
                .any(|row| &row.unit == unit && row.source == retained)
        );
    }
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.source.starts_with("opaque: "))
    );
    let duplicates = found
        .declarations
        .iter()
        .filter(|row| &row.unit == unit && row.source.starts_with("repeated: "))
        .map(|row| &row.symbol)
        .collect::<BTreeSet<_>>();
    assert!(duplicates.len() <= 1);
}

#[test]
fn javascript_array_records_keep_separate_written_properties() {
    const SOURCE: &str = concat!(
        "export const control = { preserved: 1 };\n",
        "export const records = [\n",
        "  { flag: true, flag: 1, nested: { field: 1 } },\n",
        "  { flag: false, [unknown()]: 2 },\n",
        "];\n",
        "export function read(index) { return records[index].flag; }\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found.validate_facts().expect("valid array property facts");
    let property = |written: &str| {
        let start = SOURCE.find(written).expect("written property");
        found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .unwrap_or_else(|| panic!("missing written property {written}"))
    };
    let first = property("flag: true");
    let repeated = property("flag: 1");
    let second = property("flag: false");
    assert_eq!(first.unit, second.unit);
    assert_eq!(first.symbol, repeated.symbol);
    assert_ne!(first.symbol, second.symbol);
    for row in [first, second] {
        let identity = rift_protocol::identity::SymbolIdentity::parse(row.symbol.as_str())
            .expect("checked array property identity");
        assert_eq!(
            identity.qualified_path().get(1).map(String::as_str),
            Some("records")
        );
    }
    assert!(
        property("preserved: 1")
            .symbol
            .as_str()
            .ends_with("/control/preserved")
    );
    assert!(
        property("field: 1")
            .symbol
            .as_str()
            .ends_with("/records/nested/field")
    );
    let computed = property("[unknown()]: 2");
    assert!(
        computed
            .symbol
            .as_str()
            .ends_with("/records/%5Bunknown()%5D")
    );
    assert_eq!(
        found
            .declarations
            .iter()
            .filter(|row| row.source == "[unknown()]: 2")
            .count(),
        1
    );
    assert!(found.edges.iter().all(|edge| {
        !edge.kinds.contains(&EdgeKind::Reads)
            || (edge.to != first.symbol && edge.to != second.symbol)
    }));
    let bounded = non_entry_lexical_binding_analysis(SOURCE, 2);
    assert_eq!(bounded.publication().declarations, found.declarations);
    assert!(
        bounded
            .publication()
            .edges
            .iter()
            .all(|edge| found.edges.contains(edge))
    );
}

#[test]
fn checked_local_assignments_keep_exact_literal_properties_under_the_written_function() {
    const SOURCE: &str = concat!(
        "export function build(ready) {\n",
        "  let token;\n",
        "  if (ready) token = { kind: 'ready', line: 1 };\n",
        "  else token = { kind: 'pending', line: 2 };\n",
        "  token = { settled: true };\n",
        "  token.entries = [{ kind: 'nested', line: 3 }];\n",
        "  return token;\n",
        "}\n",
    );
    let analysis = non_entry_lexical_binding_analysis(SOURCE, 64);
    let found = analysis.publication();
    found
        .validate_facts()
        .expect("complete authored declarations");
    let function = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/build"))
        .expect("named function");
    let mut identities = BTreeSet::new();
    for text in [
        "kind: 'ready'",
        "line: 1",
        "kind: 'pending'",
        "line: 2",
        "kind: 'nested'",
        "line: 3",
    ] {
        let start = SOURCE.find(text).expect("written property");
        let row = found
            .declarations
            .iter()
            .find(|row| {
                row.unit == function.unit
                    && row.range == module_runtime_range(start, text)
                    && row.source_complete
                    && row.source == text
                    && row.syntax_element_kind.as_deref() == Some("pair")
            })
            .expect("exact literal property");
        assert!(identities.insert(&row.symbol));
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&function.symbol)
        }));
    }
    assert_eq!(
        found
            .declarations
            .iter()
            .filter(|row| row.source == "token")
            .count(),
        1
    );
    assert!(
        !found
            .declarations
            .iter()
            .any(|row| row.source == "token.entries")
    );
    for source in [
        SOURCE.replace("  let token;\n", ""),
        SOURCE.replace("token.entries =", "token['entries'] ="),
        SOURCE.replace("token = { settled: true };", "token = ready;"),
        SOURCE.replace(
            "token = { settled: true };",
            "if (ready) { token = { settled: true }; }",
        ),
        SOURCE.replace(
            "  token.entries = [{ kind: 'nested', line: 3 }];",
            "  { let token; token.entries = [{ kind: 'nested', line: 3 }]; }",
        ),
    ] {
        let control = non_entry_lexical_binding_analysis(&source, 64);
        assert!(
            !control
                .publication()
                .declarations
                .iter()
                .any(|row| row.source == "kind: 'nested'")
        );
    }
}

#[test]
fn commonjs_named_export_arrays_keep_properties_under_their_written_slots() {
    const SOURCE: &str = concat!(
        "const control = { preserved: 1 };\n",
        "exports.control = control;\n",
        "exports.rows = [\n",
        "  { direct: 1, branch: { inner: 11 } },\n",
        "  { direct: 2, branch: { inner: 22 } },\n",
        "];\n",
        "module.exports.more = [{ checked: 3 }, { checked: 4 }];\n",
        "exports.read = function (index) { return exports.rows[index].direct; };\n",
    );
    const INDIRECT: &str = concat!(
        "const alias = choose(exports);\n",
        "alias.rows = [{ opaque: 1 }, { opaque: 2 }];\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.cjs", "exports.data = require('./array.cjs'); exports.indirect = require('./indirect.cjs');"),
            ("array.cjs", SOURCE),
            ("indirect.cjs", INDIRECT),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.cjs"}"#,
        )),
    )
    .expect("checked CommonJS sources");
    let found = analysis.publication();
    found.validate_facts().expect("valid array property facts");
    let unit = found
        .units
        .iter()
        .find(|unit| unit.path.0 == "array.cjs")
        .expect("array source");
    let property = |written: &str| {
        let start = SOURCE.find(written).expect("written property");
        found
            .declarations
            .iter()
            .find(|row| {
                row.unit == unit.unit
                    && row.source == written
                    && row.range == module_runtime_range(start, written)
            })
            .unwrap_or_else(|| panic!("missing written property {written}"))
    };
    for (owner, first, second) in [
        ("rows", "direct: 1", "direct: 2"),
        ("rows", "branch: { inner: 11 }", "branch: { inner: 22 }"),
        ("more", "checked: 3", "checked: 4"),
    ] {
        let left = property(first);
        let right = property(second);
        assert_ne!(left.symbol, right.symbol);
        for row in [left, right] {
            assert!(row.symbol.as_str().contains(&format!("/{owner}/")));
        }
    }
    for (parent, child) in [
        ("branch: { inner: 11 }", "inner: 11"),
        ("branch: { inner: 22 }", "inner: 22"),
    ] {
        let parent = property(parent);
        let child = property(child);
        assert!(
            found
                .objects
                .iter()
                .any(|row| row.id.as_ref() == Some(&child.symbol)
                    && row.container.as_ref() == Some(&parent.symbol))
        );
    }
    assert!(
        property("preserved: 1")
            .symbol
            .as_str()
            .ends_with("/control/preserved")
    );
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.source.starts_with("opaque: "))
    );
    assert!(
        found
            .edges
            .iter()
            .all(|edge| !edge.kinds.contains(&EdgeKind::Reads)
                || !edge.to.as_str().contains("/rows/direct"))
    );
}

#[test]
fn javascript_direct_literal_returns_keep_their_checked_object_and_initializer_owners() {
    const SOURCE: &str = concat!(
        "function named(first) {\n",
        "  if (first) return { control: 1 };\n",
        "  return { control: 2 };\n",
        "}\n",
        "const choose = (first) => {\n",
        "  if (first) return { arrow: 1 };\n",
        "  return { arrow: 2 };\n",
        "};\n",
        "exports.api = {\n",
        "  read(first) {\n",
        "    if (first) return { state: 1, child: { trace: 3 } };\n",
        "    return { state: 2, child: { trace: 4 } };\n",
        "  },\n",
        "  [computed()](first) {\n",
        "    if (first) return { hidden: 1 };\n",
        "    return { hidden: 2 };\n",
        "  },\n",
        "};\n",
        "exports.named = named;\n",
        "exports.choose = choose;\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.cjs", "exports.api = require('./api.cjs');"),
            ("api.cjs", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.cjs"}"#,
        )),
    )
    .expect("checked JavaScript sources");
    let found = analysis.publication();
    found.validate_facts().expect("valid written properties");
    let declaration = |written: &str| {
        let start = SOURCE.find(written).expect("authored property");
        found
            .declarations
            .iter()
            .find(|row| row.source == written && row.range == module_runtime_range(start, written))
            .unwrap_or_else(|| panic!("missing authored property {written}"))
    };
    for (first, second, owner) in [
        ("control: 1", "control: 2", "/named/control"),
        ("arrow: 1", "arrow: 2", "/choose/arrow"),
        ("state: 1", "state: 2", "/api/read/state"),
        (
            "child: { trace: 3 }",
            "child: { trace: 4 }",
            "/api/read/child",
        ),
    ] {
        let first = declaration(first);
        let second = declaration(second);
        assert_eq!(first.unit, second.unit);
        assert_ne!(first.symbol, second.symbol);
        assert!(first.symbol.as_str().contains(owner));
        assert!(second.symbol.as_str().contains(owner));
    }
    for (parent, child) in [
        ("child: { trace: 3 }", "trace: 3"),
        ("child: { trace: 4 }", "trace: 4"),
    ] {
        let parent = declaration(parent);
        let child = declaration(child);
        assert_eq!(parent.unit, child.unit);
        assert_eq!(
            child
                .symbol
                .as_str()
                .rsplit_once('/')
                .map(|(parent, _)| parent),
            Some(parent.symbol.as_str())
        );
    }
    assert!(
        found
            .declarations
            .iter()
            .all(|row| !row.source.starts_with("hidden:"))
    );
}

const CALLBACK_CALL_SOURCE: &str = concat!(
    "export function leaf() { return 1; }\n",
    "function schedule(task) { return task(); }\n",
    "schedule(() => leaf());\n",
    "schedule(function () { return leaf(); });\n",
    "export function enclosing() {\n",
    "  return schedule((() => leaf())) + schedule(function () { return leaf(); });\n",
    "}\n",
    "export const stable = () => leaf();\n",
    "schedule(stable);\n",
    "schedule(function named() { return leaf(); });\n",
    "schedule(() => (() => leaf()));\n",
    "schedule(() => missing());\n",
);

#[test]
fn ecmascript_anonymous_arguments_own_their_exact_inner_calls() {
    for (language, file, selector) in [
        (ShippedLanguage::JavaScript, "index.js", "main"),
        (ShippedLanguage::TypeScript, "index.ts", "types"),
    ] {
        let manifest = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"module","{selector}":"./{file}"}}"#
        );
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(file, CALLBACK_CALL_SOURCE)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("checked ECMAScript source");
        let (found, files, semantics, _) = analysis.into_parts();
        found.validate_facts().expect("valid callback call facts");
        let unit = &found.units[0].unit;
        let ranges = CALLBACK_CALL_SOURCE
            .match_indices("leaf()")
            .skip(1)
            .map(|(start, text)| TextRange {
                start: u64::try_from(start).expect("source byte offset"),
                end: u64::try_from(start + text.len()).expect("source byte end"),
            })
            .collect::<Vec<_>>();
        let calls_at = |range: &TextRange| {
            found
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds == [EdgeKind::Calls]
                        && edge.to.as_str().ends_with("/leaf")
                        && edge
                            .occurrence
                            .as_ref()
                            .is_some_and(|site| site.unit == *unit && site.range == *range)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ranges.len(), 7);
        let mut callers = BTreeSet::new();
        for range in &ranges[..4] {
            let calls = calls_at(range);
            let [call] = calls.as_slice() else {
                panic!("one inner Call at {range:?} in {file}: {calls:?}");
            };
            assert!(callers.insert(call.from.clone()));
            let callback = found
                .declarations
                .iter()
                .find(|declaration| declaration.symbol == call.from)
                .expect("anonymous argument declaration");
            assert_eq!(&callback.unit, unit);
            assert!(
                callback.source.starts_with("() =>") || callback.source.starts_with("function (")
            );
            assert!(callback.range.start < range.start && range.end <= callback.range.end);
            let object = found
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&call.from))
                .expect("callback object");
            assert!(object.document_local && object.kinds.contains(&SymbolKind::Callable));
        }
        assert!(
            calls_at(&ranges[4])
                .iter()
                .any(|call| call.from.as_str().ends_with("/stable"))
        );
        for range in &ranges[5..] {
            assert!(
                calls_at(range)
                    .iter()
                    .all(|call| !callers.contains(&call.from))
            );
        }
        assert_eq!(
            found
                .objects
                .iter()
                .filter(|object| object.name.starts_with("$callback#")
                    && object.kinds.contains(&SymbolKind::Callable))
                .count(),
            callers.len()
        );
        assert!(
            found
                .edges
                .iter()
                .all(|edge| !edge.to.as_str().ends_with("/missing"))
        );
        assert_callback_edge_capacity(&found, &files, &semantics, &callers, ranges[2].start);
    }
}

fn assert_callback_edge_capacity(
    found: &PackagePublication,
    files: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    callers: &BTreeSet<SymbolId>,
    first_enclosed: u64,
) {
    let enclosing = found
        .objects
        .iter()
        .find_map(|row| {
            row.id
                .as_ref()
                .filter(|id| id.as_str().ends_with("/enclosing"))
        })
        .expect("named enclosing callable");
    let owner = SymbolIdentity::parse(enclosing.as_str()).expect("callable identity");
    let admitted = found
        .edges
        .iter()
        .filter(|edge| {
            !callers.contains(&edge.from)
                || edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range.start >= first_enclosed)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut earlier = admitted.clone();
    for edge in &mut earlier {
        if callers.contains(&edge.from) {
            edge.from.clone_from(enclosing);
        }
    }
    let mut records = Records {
        units: found.units.clone(),
        objects: found
            .objects
            .iter()
            .filter(|row| row.id.as_ref().is_none_or(|id| !callers.contains(id)))
            .cloned()
            .collect(),
        declarations: found
            .declarations
            .iter()
            .filter(|row| !callers.contains(&row.symbol))
            .cloned()
            .collect(),
        edges: earlier,
        ..Records::default()
    };
    let limits = ExactPackageLimits::new(4096, 4096)
        .with_edges(admitted.len())
        .expect("edge bound");
    records
        .retain_ecmascript_callback_edges(owner.owner(), files, semantics, limits, EdgeKind::Calls)
        .expect("bounded calls");
    assert_eq!(records.edges, admitted);
    assert_eq!(
        records
            .declarations
            .iter()
            .filter(|row| callers.contains(&row.symbol))
            .count(),
        2
    );
}

const ANONYMOUS_SIGNATURE_TYPE_SOURCE: &str = concat!(
    "export interface Reader {\n",
    "  <Value>(value: Value): Value;\n",
    "  <Value, Option>(value: Value, option: Option): Option;\n",
    "  named<Value>(value: Value): Value;\n",
    "}\n",
    "export type Select = <Picked>(chosen: Picked) => Picked;\n",
    "export interface Factory {\n",
    "  new <Created>(created: Created): { value: Created };\n",
    "}\n",
);

#[test]
fn typescript_anonymous_signatures_keep_each_written_type_parameter_and_use() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", ANONYMOUS_SIGNATURE_TYPE_SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#,
        )),
    )
    .expect("checked TypeScript declarations");
    let publication = analysis.publication();
    publication
        .validate_facts()
        .expect("valid type parameter facts");
    let unit = &publication.units[0].unit;
    let mut type_parameters = BTreeSet::new();
    for (signature, name) in [
        ("<Value>(value: Value): Value", "Value"),
        (
            "<Value, Option>(value: Value, option: Option): Option",
            "Value",
        ),
        (
            "<Value, Option>(value: Value, option: Option): Option",
            "Option",
        ),
        ("<Picked>(chosen: Picked) => Picked", "Picked"),
        (
            "new <Created>(created: Created): { value: Created }",
            "Created",
        ),
    ] {
        let start = ANONYMOUS_SIGNATURE_TYPE_SOURCE
            .find(signature)
            .expect("authored signature");
        let mut sites = signature
            .match_indices(name)
            .map(|(at, written)| module_runtime_range(start + at, written));
        let declaration = sites.next().expect("type parameter name");
        let row = publication
            .declarations
            .iter()
            .find(|row| row.unit == *unit && row.source == name && row.range == declaration)
            .unwrap_or_else(|| panic!("missing written type parameter {name} at {declaration:?}"));
        assert!(type_parameters.insert(&row.symbol));
        assert!(
            publication
                .objects
                .iter()
                .any(|symbol| symbol.id.as_ref() == Some(&row.symbol)
                    && symbol.kinds.contains(&SymbolKind::TypeParameter))
        );
        for use_site in sites {
            let matches = publication
                .edges
                .iter()
                .filter(|edge| {
                    edge.to == row.symbol
                        && edge.kinds == [EdgeKind::HasType]
                        && edge.occurrence.as_ref().is_some_and(|occurrence| {
                            occurrence.unit == *unit && occurrence.range == use_site
                        })
                })
                .count();
            assert_eq!(matches, 1, "{name} use at {use_site:?}");
        }
    }
    assert!(
        publication
            .objects
            .iter()
            .any(|symbol| symbol.name == "value" && symbol.kinds.contains(&SymbolKind::Parameter))
    );
    assert!(
        publication
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/Reader/named/Value"))
    );
    assert!(
        publication
            .edges
            .iter()
            .all(|edge| !type_parameters.contains(&edge.to)
                || !matches!(
                    edge.kinds.as_slice(),
                    [EdgeKind::Reads | EdgeKind::Writes | EdgeKind::Calls]
                ))
    );
}

#[test]
fn javascript_nested_array_elements_keep_checked_property_owners() {
    const SOURCE: &str = concat!(
        "export const rows = [{\n",
        "  direct: [{ kept: 1 }, { kept: 2 }],\n",
        "  tuple: [['left', { leaf: 3 }], ['right', { leaf: 4 }]],\n",
        "  deep: [[[{ entry: 5 }, { entry: 6 }]]],\n",
        "  wrapped: [...[[{ hidden: 7 }, { hidden: 8 }]]],\n",
        "  sibling: 9,\n",
        "}];\n",
        "export function withheld(key) {\n",
        "  return [{ [key]: [[{ opaque: 10 }], [{ opaque: 11 }]] }];\n",
        "}\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", "export { rows } from './data.js';"),
            ("data.js", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#,
        )),
    )
    .expect("checked nested array elements");
    let found = analysis.publication();
    found.validate_facts().expect("valid property declarations");
    let unit = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "data.js")
        .expect("nested array source")
        .unit;
    let mut identities = BTreeSet::new();
    for (written, owner) in [
        ("kept: 1", "direct"),
        ("kept: 2", "direct"),
        ("leaf: 3", "tuple"),
        ("leaf: 4", "tuple"),
        ("entry: 5", "deep"),
        ("entry: 6", "deep"),
    ] {
        let start = SOURCE.find(written).expect("written property");
        let range = module_runtime_range(start, written);
        let row = found
            .declarations
            .iter()
            .find(|row| &row.unit == unit && row.range == range && row.source == written)
            .unwrap_or_else(|| panic!("missing written array element property {written}"));
        let identity = SymbolIdentity::parse(row.symbol.as_str()).expect("property identity");
        let path = identity.qualified_path();
        assert_eq!(path[path.len() - 2], owner);
        assert!(identities.insert(row.symbol.clone()));
    }
    assert!(
        found
            .declarations
            .iter()
            .any(|row| &row.unit == unit && row.source == "sibling: 9")
    );
    assert!(
        found.declarations.iter().all(|row| {
            !row.source.starts_with("hidden: ") && !row.source.starts_with("opaque: ")
        })
    );
}

#[test]
fn typescript_relative_reexports_keep_checked_declarations_beside_runtime_wildcards() {
    const ROOT: &str = "export { read } from './branch/index.js';";
    const BRANCH: &str = "export * from '../detail';";
    const DETAIL: &str = "export declare function read(input: string): string;";
    const UNUSED: &str = "export declare function unused(input: string): string;";
    const RUNTIME: &str = "export function kept() { return 1; }";
    for (routes, alternate, admitted) in [
        (r#""./*.js":"./*.js","./*":"./*.js""#, None, true),
        (
            r#""./detail.d.ts":{"types":"./other.d.ts"},"./*":"./*.js""#,
            None,
            false,
        ),
        (
            r#""./*":"./*.js""#,
            Some(("detail.d.ts.ts", "export const collision = 1;")),
            false,
        ),
        (
            r#""./*":"./*.js""#,
            Some(("detail.ts", "export const collision = 1;")),
            false,
        ),
        (
            r#""./*":{"types":"./types/*","default":"./*.js"}"#,
            None,
            false,
        ),
    ] {
        let manifest = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"module","exports":{{".":{{"types":"./index.d.ts","default":"./index.js"}},{routes}}}}}"#
        );
        let mut files = vec![
            ("index.d.ts", ROOT),
            ("branch/index.d.ts", BRANCH),
            ("detail.d.ts", DETAIL),
            ("unused.d.ts", UNUSED),
            ("other.d.ts", "export declare const marker: string;"),
            ("index.js", RUNTIME),
        ];
        files.extend(alternate);
        let collected = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            files,
            None,
            Some(("package.json", &manifest)),
        )
        .expect("checked package");
        let publication = collected.publication();
        publication.validate_facts().expect("valid package facts");
        let unit = |path| {
            publication
                .units
                .iter()
                .find(|unit| unit.path.0 == path)
                .expect("retained source")
        };
        let detail = unit("detail.d.ts");
        assert!(detail.source_complete);
        assert_eq!(detail.source, DETAIL);
        let declaration = publication
            .declarations
            .iter()
            .find(|row| row.unit == detail.unit && row.symbol.as_str().ends_with("/read"));
        assert_eq!(declaration.is_some(), admitted, "{manifest}");
        if let Some(row) = declaration {
            let start = DETAIL.find("function read").expect("written declaration");
            assert_eq!(row.source, &DETAIL[start..]);
            assert_eq!(row.range, module_runtime_range(start, &DETAIL[start..]));
            assert!(
                publication.declarations.iter().any(|parameter| {
                    parameter.unit == detail.unit
                        && parameter.symbol.as_str().ends_with("/read/input")
                        && parameter.source == "input"
                }),
                "written parameter"
            );
        }
        assert!(
            publication
                .declarations
                .iter()
                .all(|row| row.unit != unit("unused.d.ts").unit),
            "unreached declarations still follow the wildcard"
        );
        assert!(
            publication.declarations.iter().any(|row| {
                row.unit == unit("index.js").unit && row.symbol.as_str().ends_with("/beacon/kept")
            }),
            "runtime entry retains its own declaration"
        );
    }
}

#[test]
fn typescript_relative_imports_keep_checked_declarations_beside_runtime_wildcards() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.d.ts","default":"./index.js"},"./*.js":"./*.js","./*":"./*.js"}}"#;
    const ROOT: &str = concat!(
        "import type { Chosen } from './detail.js';\n",
        "import type * as settings from './options.js';\n",
        "import type { Missing } from './missing.js';\n",
        "import type { Collision } from './collision.js';\n",
        "export interface Box { item: Chosen; options: settings.Options }\n",
        "declare module 'ambient' { import type { Unreached } from './unreached.js'; }\n",
    );
    const DETAIL: &str =
        "import type { Leaf } from './leaf.js';\nexport interface Chosen { value: Leaf }";
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.d.ts", ROOT),
            ("index.js", "export const kept = 1;"),
            ("detail.d.ts", DETAIL),
            ("detail.js", "export const kept = 2;"),
            ("options.d.ts", "export interface Options { value: number }"),
            ("leaf.d.ts", "export interface Leaf { value: number }"),
            (
                "collision.d.ts",
                "export interface Collision { value: number }",
            ),
            (
                "collision.ts",
                "export interface Collision { value: string }",
            ),
            (
                "unreached.d.ts",
                "export interface Unreached { value: number }",
            ),
        ],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("checked package");
    let publication = result.publication();
    publication.validate_facts().expect("valid package facts");
    let unit = |path| {
        publication
            .units
            .iter()
            .find(|row| row.path.0 == path && row.source_complete)
            .expect("retained source")
    };
    let edge = |path, source: &str, spelling: &str, from: &str, to: &str, kind| {
        let start = source.rfind(spelling).expect("written use");
        let range = module_runtime_range(start, spelling);
        publication.edges.iter().any(|row| {
            row.kinds == [kind]
                && row.from.as_str().ends_with(from)
                && row.to.as_str().ends_with(to)
                && row
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == unit(path).unit && site.range == range)
        })
    };
    for (path, source, specifier, from, to) in [
        (
            "index.d.ts",
            ROOT,
            "'./detail.js'",
            "/beacon",
            "%2Fdetail.d.ts",
        ),
        (
            "index.d.ts",
            ROOT,
            "'./options.js'",
            "/beacon",
            "%2Foptions.d.ts",
        ),
        (
            "detail.d.ts",
            DETAIL,
            "'./leaf.js'",
            "%2Fdetail.d.ts",
            "%2Fleaf.d.ts",
        ),
    ] {
        assert!(
            edge(path, source, specifier, from, to, EdgeKind::Imports),
            "{path}: {specifier}"
        );
    }
    assert!(edge(
        "index.d.ts",
        ROOT,
        "Chosen",
        "/Box/item",
        "%2Fdetail.d.ts/Chosen",
        EdgeKind::HasType
    ));
    assert!(edge(
        "detail.d.ts",
        DETAIL,
        "Leaf",
        "/Chosen/value",
        "%2Fleaf.d.ts/Leaf",
        EdgeKind::HasType
    ));
    assert!(edge(
        "index.d.ts",
        ROOT,
        "settings.Options",
        "/Box/options",
        "%2Foptions.d.ts/Options",
        EdgeKind::HasType
    ));
    for path in ["collision.d.ts", "unreached.d.ts"] {
        assert!(
            publication
                .declarations
                .iter()
                .all(|row| row.unit != unit(path).unit),
            "{path}"
        );
    }
    assert!(publication.declarations.iter().any(|row| {
        row.unit == unit("index.js").unit && row.symbol.as_str().ends_with("/beacon/kept")
    }));
}

#[test]
fn npm_package_imports_keep_runtime_calls_and_declared_type_ranges() {
    const RUNTIME: &str = "import { Flags, select } from '#items/state';\nexport function run() { return select(Flags.enabled); }\n";
    const TYPES: &str = "import type { State } from '#items/state';\nexport declare function run(state: State): State;\n";
    const TARGET: &str =
        "export const Flags = { enabled: 1 };\nexport function select(value) { return value; }\n";
    const DECLARATIONS: &str = concat!(
        "export interface State { enabled: number }\n",
        "export declare const Flags: State;\n",
        "export declare function select(value: number): number;\n",
    );
    for (imports, javascript, typescript) in [
        (
            r##""#items/*":{"types":"./types/*.d.ts","default":"./runtime/*.js"}"##,
            true,
            true,
        ),
        (
            r##""#items/state":null,"#items/*":{"types":"./types/*.d.ts","default":"./runtime/*.js"}"##,
            false,
            false,
        ),
        (r##""#items/*":{"browser":"./runtime/*.js"}"##, false, false),
        (r##""#items/*":"../runtime/*.js""##, false, false),
        (
            r##""#items/*":{"types":"./types/*.d.ts","default":"./runtime/*.js"},"#items/state":"./unknown.js""##,
            false,
            false,
        ),
    ] {
        let manifest = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"module","exports":{{".":{{"types":"./index.d.ts","default":"./index.js"}}}},"imports":{{{imports}}}}}"#
        );
        let result = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.js", RUNTIME),
                ("index.d.ts", TYPES),
                ("runtime/state.js", TARGET),
                ("types/state.d.ts", DECLARATIONS),
            ],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("checked package files");
        let publication = result.publication();
        publication.validate_facts().expect("valid package facts");
        let origin = |path| {
            &publication
                .units
                .iter()
                .find(|unit| unit.path.0 == path)
                .expect("retained source")
                .unit
        };
        let written = |source: &str, marker: &str, token: &str| {
            let start = source.find(marker).expect("written use");
            module_runtime_range(start, token)
        };
        let calls = |target, range, unit| {
            publication.edges.iter().any(|row| {
                row.kinds == [EdgeKind::Calls]
                    && row.to.as_str().ends_with(target)
                    && row
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| &site.unit == unit && site.range == range)
            })
        };
        assert_eq!(
            calls(
                "runtime%2Fstate.js/select",
                written(RUNTIME, "select(Flags.enabled)", "select(Flags.enabled)"),
                origin("index.js"),
            ),
            javascript,
            "{manifest}",
        );
        assert_eq!(
            publication.edges.iter().any(|row| {
                row.kinds == [EdgeKind::Reads]
                    && row.to.as_str().ends_with("runtime%2Fstate.js/Flags")
                    && row.occurrence.as_ref().is_some_and(|site| {
                        &site.unit == origin("index.js")
                            && site.range == written(RUNTIME, "Flags.enabled", "Flags")
                    })
            }),
            javascript,
            "{manifest}",
        );
        let type_uses = publication
            .edges
            .iter()
            .filter(|row| {
                row.kinds == [EdgeKind::HasType]
                    && row.to.as_str().ends_with("types%2Fstate.d.ts/State")
                    && row.occurrence.as_ref().is_some_and(|site| {
                        &site.unit == origin("index.d.ts")
                            && site.range.start
                                >= u64::try_from(TYPES.find("run(").expect("function"))
                                    .expect("source range")
                    })
            })
            .count();
        assert_eq!(type_uses, usize::from(typescript) * 2, "{manifest}");
    }
}

#[test]
fn typescript_declared_function_type_parameters_keep_physical_signature_uses() {
    const SOURCE: &str = concat!(
        "declare function plain<T>(value: T): T;\n",
        "export { plain };\n",
        "export declare function choose<Value>(value: Value[]): Value;\n",
        "export declare function choose<Value>(value: Value): Value;\n",
        "export declare function own<Type>(value: Type): Type;\n",
        "export declare function ambiguous<U, U>(value: U): U;\n",
    );
    const IMPLEMENTATION: &str = "export function implemented<T>(value: T): T { return value; }\n";
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", SOURCE), ("complete.ts", IMPLEMENTATION)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.d.ts"}"#,
        )),
    )
    .expect("checked declared signatures");
    let found = result.publication();
    found.validate_facts().expect("checked ranges and targets");
    let index = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.d.ts")
        .expect("declaration unit")
        .unit;
    let mut identities = BTreeSet::new();
    let mut line_start = 0;
    for (line, (owner, name)) in SOURCE.lines().zip([
        ("plain", "T"),
        ("", ""),
        ("choose", "Value"),
        ("choose", "Value"),
        ("own", "Type"),
    ]) {
        if !owner.is_empty() {
            let declaration = format!("{owner}<{name}>");
            let start =
                line_start + line.find(&declaration).expect("written owner") + owner.len() + 1;
            let row = found
                .declarations
                .iter()
                .find(|row| {
                    row.unit == *index
                        && row.source == name
                        && row.range == module_runtime_range(start, name)
                })
                .expect("written type parameter in declared signature");
            assert!(identities.insert(&row.symbol));
            let expected = line
                .match_indices(name)
                .skip(1)
                .map(|(offset, _)| {
                    let range = module_runtime_range(line_start + offset, name);
                    (range.start, range.end)
                })
                .collect::<BTreeSet<_>>();
            let actual = found
                .edges
                .iter()
                .filter_map(|edge| {
                    let site = edge.occurrence.as_ref()?;
                    (edge.to == row.symbol
                        && edge.kinds == [EdgeKind::HasType]
                        && edge.from.as_str().ends_with(&format!("/{owner}"))
                        && site.unit == *index)
                        .then_some((site.range.start, site.range.end))
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(actual, expected, "{line}");
        }
        line_start += line.len() + 1;
    }
    assert!(
        found
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/implemented/T"))
    );
    assert!(
        found
            .declarations
            .iter()
            .all(|row| row.source != "U" || row.unit != *index)
    );
}

#[test]
fn typescript_inferred_names_only_bind_their_checked_true_branches() {
    const SOURCE: &str = concat!(
        "export type Result = string;\n",
        "export interface Kept<Value> { value: Value }\n",
        "export type ValueOf<T> = T extends { value: infer Value } ? Value : never;\n",
        "export type ItemOf<T> = T extends [infer Item, ...unknown[]] ? Item : never;\n",
        "export type Read<T> = T extends { read(): infer Result } ? Result : Result;\n",
        "export type Nested<T> = T extends [infer Item] ? (Item extends { value: infer Item } ? Item : Item) : never;\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"./index.d.ts"}"#,
        )),
    )
    .expect("complete package declaration");
    let found = result.publication();
    found.validate_facts().expect("checked conditional types");
    let unit = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.d.ts")
        .expect("physical declaration")
        .unit;
    let declaration = |prefix: &str, name: &str| {
        let begin = SOURCE.find(prefix).expect("written prefix") + prefix.len();
        found
            .declarations
            .iter()
            .find(|row| {
                row.unit == *unit
                    && row.source == name
                    && row.range == module_runtime_range(begin, name)
            })
            .unwrap_or_else(|| panic!("written inference {prefix}{name} at {begin}"))
    };
    let edges = |row: &rift_protocol::index::PackageSymbol| {
        found
            .edges
            .iter()
            .filter_map(|edge| {
                let site = edge.occurrence.as_ref()?;
                (edge.to == row.symbol && edge.kinds == [EdgeKind::HasType] && site.unit == *unit)
                    .then_some((site.range.start, site.range.end))
            })
            .collect::<BTreeSet<_>>()
    };
    let written_use = |prefix: &str, name: &str| {
        let begin = SOURCE.find(prefix).expect("written use") + prefix.len();
        let range = module_runtime_range(begin, name);
        (range.start, range.end)
    };
    for (decl, usage, name) in [
        ("value: infer ", "infer Value } ? ", "Value"),
        ("extends [infer ", "infer Item, ...unknown[]] ? ", "Item"),
        ("read(): infer ", "infer Result } ? ", "Result"),
    ] {
        assert_eq!(
            edges(declaration(decl, name)),
            BTreeSet::from([written_use(usage, name)])
        );
    }
    let ordinary = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/beacon/Result"))
        .expect("outer type alias");
    assert!(edges(ordinary).contains(&written_use("infer Result } ? Result : ", "Result")));
    let outer = declaration("export type Nested<T> = T extends [infer ", "Item");
    let inner = declaration("(Item extends { value: infer ", "Item");
    assert_ne!(outer.symbol, inner.symbol);
    assert_eq!(
        edges(outer),
        BTreeSet::from([
            written_use("Nested<T> = T extends [infer Item] ? (", "Item"),
            written_use("value: infer Item } ? Item : ", "Item"),
        ])
    );
    assert_eq!(
        edges(inner),
        BTreeSet::from([written_use("value: infer Item } ? ", "Item")])
    );
    assert!(
        found
            .declarations
            .iter()
            .any(|row| row.symbol.as_str().ends_with("/Kept/Value"))
    );
}

#[test]
fn javascript_assignments_keep_distinct_checked_member_literal_owners() {
    const SOURCE: &str = concat!(
        "export function set(key) {\n",
        "    const state = { errors: [] };\n",
        "    const check = [{ found: 1, visible: { member: 2 } }];\n",
        "    state.errors = [{ params: { additionalProperty: 3 } }];\n",
        "    state[key] = [{ params: { additionalProperty: 5 } }];\n",
        "    return (state.errors = [{ params: { additionalProperty: 4 } }], check);\n",
        "}\n",
        "export function replaced(value) {\n",
        "    let holder = { errors: [] };\n",
        "    holder = value;\n",
        "    holder.errors = [{ withheld: 6 }];\n",
        "    return (holder.errors = [{ withheld: 7 }], holder);\n",
        "}\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", "export { set } from './data.js';"),
            ("data.js", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js"}"#,
        )),
    )
    .expect("checked local member assignments");
    let found = analysis.publication();
    found.validate_facts().expect("valid literal owners");
    let unit = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "data.js")
        .expect("written module")
        .unit;
    let declaration = |written: &str| {
        let range = module_runtime_range(SOURCE.find(written).expect("written site"), written);
        found
            .declarations
            .iter()
            .find(|row| &row.unit == unit && row.range == range && row.source == written)
            .unwrap_or_else(|| panic!("missing checked literal property {written}"))
    };
    let first = declaration("params: { additionalProperty: 3 }");
    let second = declaration("params: { additionalProperty: 4 }");
    assert_ne!(first.symbol, second.symbol);
    for (written, parent) in [
        ("additionalProperty: 3", first),
        ("additionalProperty: 4", second),
    ] {
        let child = declaration(written);
        let symbol = found
            .objects
            .iter()
            .find(|symbol| symbol.id.as_ref() == Some(&child.symbol))
            .expect("authored nested property");
        assert_eq!(symbol.container.as_ref(), Some(&parent.symbol));
    }
    assert!(
        declaration("errors: []")
            .symbol
            .as_str()
            .ends_with("/set/state/errors")
    );
    declaration("found: 1");
    declaration("member: 2");
    assert!(found.declarations.iter().all(|row| !matches!(
        row.source.as_str(),
        "additionalProperty: 5" | "withheld: 6" | "withheld: 7"
    )));
}

#[test]
fn javascript_callback_arguments_keep_checked_runtime_member_reads() {
    const SOURCE: &str = concat!(
        "import { Reader } from './index.js';\n",
        "function schedule(task) { return task(); }\n",
        "export function named() { return Reader.tag; }\n",
        "schedule(() => Reader.tag);\n",
        "schedule(function () { return Reader.tag; });\n",
        "schedule(() => (new Reader(), Reader.tag));\n",
        "export function shadow(Reader) { schedule(() => Reader.tag); }\n",
        "Reader.tag;\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", "export { Reader } from './reader.js';"),
            ("index.d.ts", "export { Reader } from './reader.js';"),
            ("reader.js", "export class Reader { static tag = 1; }"),
            ("reader.d.ts", "export declare class Reader { static tag: number; }"),
            ("reader-use.js", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.d.ts","import":"./index.js"}}}"#,
        )),
    )
    .expect("checked callback package");
    let (found, files, semantics, _) = result.into_parts();
    found.validate_facts().expect("physical callback owners");
    let unit = &found
        .units
        .iter()
        .find(|unit| unit.path.0 == "reader-use.js")
        .expect("written callback source")
        .unit;
    let written_pairs = SOURCE.match_indices("Reader.tag").collect::<Vec<_>>();
    assert_eq!(written_pairs.len(), 6);
    let read_owner = |position, spelling, target| {
        let range = module_runtime_range(position, spelling);
        let mut reads = found.edges.iter().filter(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to.as_str().ends_with(target)
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == *unit && site.range == range)
        });
        let first = reads.next();
        assert!(reads.next().is_none());
        first.map(|edge| &edge.from)
    };
    let runtime_type = "/javascript/beacon%2Freader.js/Reader";
    let runtime_property = "/javascript/beacon%2Freader.js/Reader/tag";
    let mut callbacks = BTreeSet::new();
    for (index, (start, _)) in written_pairs.iter().copied().enumerate() {
        let qualifier = read_owner(start, "Reader", runtime_type);
        let member = read_owner(start, "Reader.tag", runtime_property);
        assert_eq!(qualifier, member);
        match index {
            0 => assert!(qualifier.expect("named owner").as_str().ends_with("/named")),
            4 => assert!(
                qualifier.is_none(),
                "the callback parameter shadows the imported class"
            ),
            5 => assert!(
                qualifier
                    .expect("module owner")
                    .as_str()
                    .ends_with("%2Freader-use.js")
            ),
            _ => {
                let id = qualifier.expect("anonymous owner");
                assert!(callbacks.insert(id));
                assert!(
                    found
                        .objects
                        .iter()
                        .any(|row| row.id.as_ref() == Some(id) && row.document_local)
                );
            }
        }
    }
    let caller =
        read_owner(written_pairs[3].0, "Reader", runtime_type).expect("shared callback owner");
    let call = module_runtime_range(
        SOURCE.find("new Reader()").expect("written constructor"),
        "new Reader()",
    );
    assert!(found.edges.iter().any(|edge| {
        &edge.from == caller
            && edge.kinds == [EdgeKind::Calls]
            && edge.to.as_str().ends_with(runtime_type)
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit == *unit && site.range == call)
    }));
    assert_callback_reads_preserve_prior_records_at_the_cap(
        &found, &files, &semantics, &callbacks, caller,
    );
}

fn assert_callback_reads_preserve_prior_records_at_the_cap(
    found: &PackagePublication,
    files: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    callbacks: &BTreeSet<&SymbolId>,
    existing: &SymbolId,
) {
    let prior_owner = |id: &SymbolId| id == existing || !callbacks.contains(id);
    let prior_objects = found
        .objects
        .iter()
        .filter(|row| row.id.as_ref().is_none_or(prior_owner))
        .cloned()
        .collect::<Vec<_>>();
    let prior_declarations = found
        .declarations
        .iter()
        .filter(|row| prior_owner(&row.symbol))
        .cloned()
        .collect::<Vec<_>>();
    let prior_edges = found
        .edges
        .iter()
        .filter(|edge| edge.kinds != [EdgeKind::Reads] || !callbacks.contains(&edge.from))
        .cloned()
        .collect::<Vec<_>>();
    let mut records = Records {
        units: found.units.clone(),
        objects: prior_objects.clone(),
        declarations: prior_declarations.clone(),
        edges: prior_edges.clone(),
        ..Records::default()
    };
    let owner = SymbolIdentity::parse(existing.as_str()).expect("existing callback identity");
    let limits = ExactPackageLimits::new(4096, 4096)
        .with_edges(prior_edges.len())
        .expect("edge bound");
    records
        .retain_ecmascript_callback_edges(owner.owner(), files, semantics, limits, EdgeKind::Reads)
        .expect("bounded callback reads");
    assert_eq!(records.edges, prior_edges);
    assert_eq!(records.objects, prior_objects);
    assert_eq!(records.declarations, prior_declarations);
}

#[test]
fn typescript_reexport_only_declaration_files_keep_their_module_imports() {
    const BARREL: &str = "export * from './leaf.js';\nexport * from './absent.js';\n";
    const LEGACY: &str = "declare interface LocalOnly { value: number }\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.js", "export const runtime = true;\n"),
            ("index.d.ts", "export * from './branch/index.js';\n"),
            ("branch/index.d.ts", BARREL),
            ("branch/leaf.d.ts", "export interface Reader { value: number }\n"),
            ("branch/leaf.js", "export const source = 1;\n"),
            ("branch/legacy.d.ts", LEGACY),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.js","types":"./index.d.ts"}"#,
        )),
    )
    .expect("written declaration barrel");
    let found = analysis.publication();
    found.validate_facts().expect("checked typescript module");
    let owner_suffix = "/typescript/beacon%2Fbranch%2Findex.d.ts";
    let module = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with(owner_suffix) && row.source == BARREL)
        .expect("complete declaration barrel owner");
    let literal = "'./leaf.js'";
    let occurrence = module_runtime_range(BARREL.find(literal).expect("specifier"), literal);
    assert!(found.edges.iter().any(|edge| {
        edge.from == module.symbol
            && edge.kinds == [EdgeKind::Imports]
            && edge
                .to
                .as_str()
                .ends_with("/typescript/beacon%2Fbranch%2Fleaf.d.ts")
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit == module.unit && site.range == occurrence)
    }));
    assert!(found.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Aliases]
            && edge.from.as_str().ends_with("index.d.ts/Reader")
            && edge.to.as_str().ends_with("leaf.d.ts/Reader")
    }));
    assert!(found.declarations.iter().all(|row| row.source != LEGACY));
    let missing = "'./absent.js'";
    let missing = module_runtime_range(BARREL.find(missing).expect("missing specifier"), missing);
    assert!(
        found
            .edges
            .iter()
            .filter(|edge| edge.from == module.symbol)
            .all(|edge| {
                !edge.to.as_str().ends_with("leaf.js")
                    && edge
                        .occurrence
                        .as_ref()
                        .is_none_or(|site| site.unit != module.unit || site.range != missing)
            })
    );
}

#[test]
fn checked_commonjs_literals_as_direct_call_arguments_keep_module_imports() {
    const SOURCE: &str = concat!(
        "const established = require('./leaf');\n",
        "function wrap(value) { return value; }\n",
        "const wrapped = wrap(require('./leaf'));\n",
        "wrap(require('./leaf'));\n",
        "wrap(wrap(require('./leaf')));\n",
        "wrap?.(require('./leaf'));\n",
        "wrap(require('./leaf', 1));\n",
        "wrap(require('./le' + 'af'));\n",
        "wrap(require('./missing'));\n",
        "wrap([require('./leaf')]);\n",
        "function shadow(require) { return wrap(require('./leaf')); }\n",
        "exports.established = established;\n",
        "exports.wrapped = wrapped;\n",
    );
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.js"}"#;
    const TARGET: &str = "exports.leaf = function leaf() { return 1; };\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", SOURCE), ("leaf.js", TARGET)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("checked CommonJS source");
    let (found, _, semantics, _) = analysis.into_parts();
    found.validate_facts().expect("valid written imports");
    let modules = |text| {
        found
            .declarations
            .iter()
            .find(|row| row.source_complete && row.source == text)
            .expect("complete physical module")
    };
    let entry = modules(SOURCE);
    let target = modules(TARGET);
    let quoted = SOURCE
        .match_indices("'./leaf'")
        .map(|(start, text)| {
            let start = u64::try_from(start).expect("bounded fixture offset");
            let size = u64::try_from(text.len()).expect("bounded literal");
            TextRange {
                start,
                end: start + size,
            }
        })
        .collect::<Vec<_>>();
    let imports = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::Imports]
            && edge.from == entry.symbol
            && edge.to == target.symbol
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit == entry.unit)
    };
    let mut written = found
        .edges
        .iter()
        .filter(|edge| imports(edge))
        .filter_map(|edge| edge.occurrence.as_ref().map(|site| site.range.clone()))
        .collect::<Vec<_>>();
    written.sort_by_key(|site| (site.start, site.end));
    assert_eq!(written, quoted[..4]);
    let mut records = Records {
        units: found.units.clone(),
        objects: found.objects.clone(),
        declarations: found.declarations.clone(),
        edges: found
            .edges
            .into_iter()
            .filter(|edge| {
                !imports(edge)
                    || edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.range == quoted[0])
            })
            .collect(),
        ..Records::default()
    };
    let earlier = records.edges.clone();
    records.retain_commonjs_argument_imports(&semantics, earlier.len());
    assert_eq!(records.edges, earlier, "incumbent capacity");
}

#[test]
fn checked_commonjs_expression_entries_keep_the_module_and_relative_load() {
    for (entry, mode, module, loader) in [
        (
            "module.exports = Object.assign(function run(value) { return value; }, { flag: true });",
            "commonjs",
            "'../'",
            true,
        ),
        (
            "if (enabled) module.exports = function run(value) { return value; };",
            "commonjs",
            "'..'",
            false,
        ),
        (
            "module.exports = Object.assign(function run(value) { return value; }, { flag: true });",
            "commonjs",
            "'../missing'",
            false,
        ),
        (
            "const Object = { assign: make }; module.exports = Object.assign(function run() {}, {});",
            "commonjs",
            "'../'",
            false,
        ),
        (
            "const module = {}; module.exports = make();",
            "commonjs",
            "'../'",
            false,
        ),
        ("module.exports = make();", "module", "'../'", false),
        (
            "if (enabled) module.exports = make();",
            "commonjs",
            "'../'",
            false,
        ),
    ] {
        let source = format!(
            "const load = require({module}); function inspect() {{ return load(1); }} inspect();"
        );
        let manifest =
            format!(r#"{{"name":"beacon","version":"1.0.0","type":"{mode}","main":"index.js"}}"#);
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", entry), ("test/check.js", &source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("checked CommonJS source");
        let publication = analysis.publication();
        publication.validate_facts().expect("complete module facts");
        let physical = |source| {
            publication.declarations.iter().find(|row| {
                row.source_complete
                    && row.source == source
                    && row.range.start == 0
                    && publication.objects.iter().any(|object| {
                        object.id.as_ref() == Some(&row.symbol)
                            && object.kinds.contains(&SymbolKind::Module)
                    })
            })
        };
        let target = physical(entry);
        let load = physical(source.as_str());
        assert_eq!(
            load.is_some(),
            loader || mode == "module",
            "{entry} {mode} {module}"
        );
        let expects_entry = loader || module == "'../missing'" || mode == "module";
        assert_eq!(target.is_some(), expects_entry, "{entry} {mode} {module}");
        let imported = target.zip(load).is_some_and(|(target, load)| {
            let start = u64::try_from(source.find(module).expect("written specifier"))
                .expect("small source");
            let end = start + u64::try_from(module.len()).expect("small specifier");
            publication.edges.iter().any(|edge| {
                edge.from == load.symbol
                    && edge.to == target.symbol
                    && edge.kinds == [EdgeKind::Imports]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == load.unit && site.range == TextRange { start, end }
                    })
            })
        });
        assert_eq!(imported, loader, "{entry} {mode} {module}");
    }
}

#[test]
fn checked_commonjs_default_function_keeps_the_physical_entry_module() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"index.js","exports":{".":[{"default":"./index.js"},"./index.js"]}}"#;
    const CLIENT: &str =
        "const base = require('../');\nfunction local() { return base(); }\nlocal();\n";
    for (entry, complete) in [
        ("module.exports = function serve() { return 1; };", true),
        (
            "function serve() { return 1; } module.exports = serve;",
            true,
        ),
        (
            "if (flag) module.exports = function serve() { return 1; };",
            false,
        ),
        (
            "module.exports = function serve() { return 1; }; module.exports = change;",
            false,
        ),
        (
            "const module = {}; module.exports = function serve() { return 1; };",
            false,
        ),
        (
            "module.exports = function serve() { return 1; }; expose(module);",
            false,
        ),
    ] {
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", entry), ("sub/client.js", CLIENT)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("selected package");
        let publication = analysis.publication();
        let source = |path: &str, written: &str| {
            publication.declarations.iter().any(|row| {
                row.unit.as_str().ends_with(path)
                    && row.range.start == 0
                    && row.source_complete
                    && row.source == written
            })
        };
        assert_eq!(source("/index.js", entry), complete, "{entry}");
        assert_eq!(source("/client.js", CLIENT), complete, "{entry}");
        assert_eq!(
            publication.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Imports]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit.as_str().ends_with("/client.js")
                            && CLIENT.get(offset_in(site.range.start)..offset_in(site.range.end))
                                == Some("'../'")
                    })
            }),
            complete,
            "{entry}"
        );
    }
}

#[test]
fn written_argument_properties_keep_the_same_callback_without_a_call_target() {
    const SOURCE: &str = concat!(
        "var check = require('checks');\n",
        "var load = require('../');\n",
        "check('first', function (observer) {\n",
        "  var directory = 'first';\n",
        "  load(directory, { path: directory }, function (error, value) { observer.equal(value, directory); });\n",
        "  load(directory, { path: directory, flag: true }, function (error, value) { observer.equal(value, directory); });\n",
        "});\n",
        "check('second', function (observer) {\n",
        "  var directory = 'second';\n",
        "  load(directory, { path: directory }, function (error, value) { observer.equal(value, directory); });\n",
        "  load(directory, { path: directory, flag: false }, function (error, value) { observer.equal(value, directory); });\n",
        "});\n",
    );
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"index.js"}"#;
    let check = |implementation, expected_calls| {
        let result = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", implementation), ("test/options.cjs", SOURCE)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("selected CommonJS source");
        let found = result.publication();
        found.validate_facts().expect("valid physical ownership");
        let unit = found
            .units
            .iter()
            .find(|file| file.path.0 == "test/options.cjs")
            .expect("complete written source");
        assert_eq!(unit.source, SOURCE);
        let mut properties = BTreeMap::new();
        for (start, written) in SOURCE.match_indices("path: directory") {
            let start = u64::try_from(start).expect("small source");
            let insert = found
                .declarations
                .iter()
                .find(|row| {
                    row.unit == unit.unit
                        && row.source_complete
                        && row.source == written
                        && row.range.start == start
                })
                .expect("written property");
            let property = found
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&insert.symbol))
                .expect("property object");
            let callback = property.container.as_ref().expect("physical callback");
            let owner = found
                .declarations
                .iter()
                .find(|row| {
                    row.symbol == *callback
                        && row.unit == unit.unit
                        && row.source_complete
                        && row.range.start < start
                        && start < row.range.end
                })
                .expect("complete callable containing literal");
            assert_eq!(
                owner.syntax_element_kind.as_deref(),
                Some("function_expression")
            );
            properties.insert(start, insert.symbol.clone());
        }
        assert_eq!(properties.len(), 4);
        assert_eq!(properties.values().collect::<BTreeSet<_>>().len(), 4);
        let calls = found
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.unit == unit.unit)
            })
            .count();
        assert_eq!(calls, expected_calls);
        properties
    };
    let resolved = check(
        "function load(path, options, callback) { return options; } module.exports = load;",
        4,
    );
    let unresolved = check(
        "function retained() { return 1; } exports.marker = retained; module.exports = unknown();",
        0,
    );
    assert_eq!(resolved, unresolved);
}

#[test]
fn typescript_enum_members_keep_references_at_their_written_qualifiers() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#;
    const SOURCE: &str = concat!(
        "import { Status as State, Details } from './codes.js';\n",
        "export type Report = State.Ready | State.Waiting;\n",
        "export type UnknownMember = State.Missing;\n",
        "export type UnknownEnum = Other.Ready;\n",
        "export type Detail = Details.Leaf;\n",
    );
    const TARGET: &str = concat!(
        "export declare enum Status { Ready = 1, Waiting = 2 }\n",
        "export declare namespace Details { type Leaf = number; }\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", SOURCE), ("codes.d.ts", TARGET)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("checked package");
    let publication = result.publication();
    publication.validate_facts().expect("valid package facts");
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.d.ts")
        .expect("retained source");
    let qualifiers = publication
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::References])
        .filter_map(|edge| {
            let site = edge.occurrence.as_ref()?;
            (site.unit == unit.unit).then_some((
                edge.from.as_str(),
                edge.to.as_str(),
                site.range.clone(),
            ))
        })
        .collect::<Vec<_>>();
    assert_eq!(qualifiers.len(), 2);
    for (member, target) in [("State.Ready", "/Ready"), ("State.Waiting", "/Waiting")] {
        let start = SOURCE.find(member).expect("written type");
        let range = module_runtime_range(start, "State");
        assert!(qualifiers.iter().any(|(from, to, site)| {
            from.ends_with("/Report") && to.ends_with("codes.d.ts/Status") && *site == range
        }));
        let range = module_runtime_range(start, member);
        let target = format!("codes.d.ts/Status{target}");
        assert!(publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::HasType]
                && edge.from.as_str().ends_with("/Report")
                && edge.to.as_str().ends_with(&target)
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == unit.unit && site.range == range)
        }));
    }
}

#[test]
fn closed_anonymous_commonjs_callable_keeps_its_exact_literal_properties() {
    const ENTRY: &str = "const build = require('./worker.js');\nmodule.exports = build;\n";
    const WORKER: &str = concat!(
        "module.exports = function (ready) {\n",
        "    const results = [];\n",
        "    if (ready) results.push({ kind: 'ready', line: 1 });\n",
        "    else results.push({ kind: 'pending', line: 2 });\n",
        "    return results;\n",
        "};\n",
    );
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#;
    let closed = |worker: &str| {
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", ENTRY), ("worker.js", worker)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("analyzed source");
        analysis
            .publication()
            .validate_facts()
            .expect("valid source");
        analysis.into_parts().0
    };
    let publication = closed(WORKER);
    let module = publication
        .declarations
        .iter()
        .find(|row| row.source_complete && row.source == WORKER)
        .expect("complete worker module");
    let callable = publication
        .declarations
        .iter()
        .find(|row| {
            row.unit == module.unit
                && row.syntax_element_kind.as_deref() == Some("assignment_expression")
                && row.symbol.as_str().ends_with("/default")
        })
        .expect("existing physical default callable");
    let mut properties = BTreeSet::new();
    for text in ["kind: 'ready'", "line: 1", "kind: 'pending'", "line: 2"] {
        let start = u64::try_from(WORKER.find(text).expect("written property")).expect("offset");
        let row = publication
            .declarations
            .iter()
            .find(|row| {
                row.unit == module.unit
                    && row.range.start == start
                    && row.source_complete
                    && row.source == text
                    && row.syntax_element_kind.as_deref() == Some("pair")
            })
            .expect("complete written property");
        assert!(properties.insert(&row.symbol));
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&callable.symbol)
        }));
    }
    for worker in [
        format!("if (enabled) {{ {WORKER} }}"),
        format!("{WORKER} if (enabled) module.exports = other;"),
        format!("{WORKER} inspect(module);"),
        WORKER.replace("function (ready)", "async function (ready)"),
    ] {
        assert!(!closed(&worker).declarations.iter().any(|row| {
            row.source == worker || row.source == "kind: 'ready'" || row.source == "line: 1"
        }));
    }
}

#[test]
fn checked_commonjs_loads_keep_reads_of_the_required_namespace() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.js"}"#;
    const SOURCE: &str = concat!(
        "const helper = require('./helper');\n",
        "function run() { return helper.apply(); }\n",
        "function shadow(helper) { return helper.apply(); }\n",
        "let changing = require('./helper');\n",
        "changing = {};\n",
        "function ignore() { return changing.apply(); }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", "module.exports = require('./helper');\n"),
            (
                "helper.js",
                "function apply() { return 7; }\nmodule.exports = { apply };\n",
            ),
            ("secondary.js", SOURCE),
        ],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("checked CommonJS sources");
    let publication = analysis.publication();
    publication.validate_facts().expect("valid facts");
    let site = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "secondary.js")
        .expect("secondary source");
    let receiver = SOURCE.find("helper.apply").expect("written receiver");
    let receiver_range = module_runtime_range(receiver, "helper");
    let reads = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to.as_str().ends_with("beacon%2Fhelper.js")
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|span| span.unit == site.unit)
        })
        .collect::<Vec<_>>();
    let [read] = reads.as_slice() else {
        panic!("expected one checked receiver Read, got {reads:?}");
    };
    assert!(read.from.as_str().ends_with("secondary.js/run"));
    assert_eq!(
        read.occurrence.as_ref().expect("receiver span").range,
        receiver_range
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from == read.from
            && edge.to.as_str().ends_with("secondary.js/helper")
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|span| span.unit == site.unit && span.range == receiver_range)
    }));
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from == read.from
            && edge.to.as_str().ends_with("helper.js/apply")
            && edge.occurrence.as_ref().is_some_and(|span| {
                span.unit == site.unit
                    && span.range == module_runtime_range(receiver, "helper.apply()")
            })
    }));
}

#[test]
fn javascript_destructuring_reads_one_checked_return_object() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.mjs"}"#;
    const PROVIDER: &str = concat!(
        "export class Outcome {\n",
        "  static explain(input) {\n",
        "    const description = input;\n",
        "    const options = {};\n",
        "    return { description, options };\n",
        "  }\n",
        "  static unclear(flag) {\n",
        "    if (flag) return { description: 'first' };\n",
        "    return { description: 'second' };\n",
        "  }\n",
        "  static duplicate(input) { return { description: input, description: 'other' }; }\n",
        "  static spreading(input) { return { description: 'first', ...input }; }\n",
        "  static computed(input) { return { description: 'first', [input]: 'other' }; }\n",
        "  static async future() { return { description: 'first' }; }\n",
        "}\n",
    );
    const CONSUMER: &str = concat!(
        "import { Outcome } from './outcome.mjs';\n",
        "function left(value) { return { description: value }; }\n",
        "function right(value) { return { description: value }; }\n",
        "export class Client extends Outcome {\n",
        "  constructor(input) {\n",
        "    const { description = 'fallback', options: settings = {} } = Outcome.explain(input);\n",
        "    const { description: second } = Outcome.unclear(input);\n",
        "    const { description: third } = Outcome.duplicate(input);\n",
        "    const { description: fourth } = Outcome.spreading(input);\n",
        "    const { description: fifth } = Outcome.computed(input);\n",
        "    const { description: sixth } = Outcome.future();\n",
        "    const { missing } = Outcome.explain(input);\n",
        "    const choose = input ? left : right;\n",
        "    const { description: seventh } = choose(input);\n",
        "    super(input);\n",
        "  }\n",
        "}\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", CONSUMER), ("outcome.mjs", PROVIDER)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("selected sources");
    let publication = result.publication();
    publication.validate_facts().expect("valid package facts");
    let caller = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.mjs")
        .expect("caller source");
    let selector_reads = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == caller.unit)
                && edge.to.as_str().rsplit('/').next().is_some_and(|name| {
                    name.starts_with("description") || name.starts_with("options")
                })
                && !edge.to.as_str().contains("/Client/constructor/")
        })
        .collect::<Vec<_>>();
    assert_eq!(selector_reads.len(), 2);
    for (pattern, key) in [
        ("description = 'fallback'", "description"),
        ("options: settings", "options"),
    ] {
        let start = CONSUMER.find(pattern).expect("written selector");
        let site = module_runtime_range(start, key);
        let target = selector_reads.iter().find(|edge| {
            edge.from.as_str().ends_with("/Client/constructor")
                && edge
                    .to
                    .as_str()
                    .contains(&format!("/Outcome/explain/{key}"))
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|span| span.range == site)
        });
        assert!(target.is_some(), "{pattern}");
    }
}

#[test]
fn checked_commonjs_default_arrow_keeps_the_selected_entry_and_relative_importer() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./lib/index.js"}"#;
    const CLIENT: &str = "const load = require('..');\nload({ active: true });";
    for (entry, complete) in [
        ("module.exports = (item) => { return item.active; };", true),
        (
            "const load = (item) => item.active; module.exports = load;",
            true,
        ),
        ("if (flag) module.exports = (item) => item.active;", false),
        (
            "module.exports = (item) => item.active; module.exports = replacement;",
            false,
        ),
        (
            "const module = {}; module.exports = (item) => item.active;",
            false,
        ),
        (
            "module.exports = (item) => item.active; expose(module);",
            false,
        ),
        ("module.exports = wrap((item) => item.active);", false),
    ] {
        let analyzed = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("lib/index.js", entry), ("test/client.js", CLIENT)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("selected module and importer");
        let publication = analyzed.publication();
        publication.validate_facts().expect("valid module facts");
        let placed = |suffix: &str, source: &str| {
            publication.declarations.iter().find(|row| {
                row.unit.as_str().ends_with(suffix)
                    && row.range.start == 0
                    && row.source_complete
                    && row.source == source
            })
        };
        let root = placed("/lib/index.js", entry);
        let client = placed("/test/client.js", CLIENT);
        assert_eq!(root.is_some(), complete, "{entry}");
        assert_eq!(client.is_some(), complete, "{entry}");
        if let Some((root, client)) = root.zip(client) {
            assert!(publication.edges.iter().any(|edge| {
                edge.from == client.symbol
                    && edge.to == root.symbol
                    && edge.kinds == [EdgeKind::Imports]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == client.unit
                            && CLIENT.get(offset_in(site.range.start)..offset_in(site.range.end))
                                == Some("'..'")
                    })
            }));
        }
    }
}

#[test]
fn typescript_declaration_namespaces_keep_local_export_selector_references() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","types":"./index.d.ts"}"#;
    const SOURCE: &str = concat!(
        "declare function build(input: Options): Options;\n",
        "declare namespace build {\n",
        "  type Shadow = { inner: string };\n",
        "  export { Options, Shadow };\n",
        "  export { Missing };\n",
        "}\n",
        "type Options = { count: number };\n",
        "type Shadow = { outer: number };\n",
        "export = build;\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", SOURCE)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("selected TypeScript source");
    let doc = result.publication();
    doc.validate_facts().expect("valid package facts");
    let unit = doc.units.first().expect("retained declaration source");
    let statement = SOURCE.find("export { Options").expect("written selectors");
    let selectors = [
        ("Options", "/Options", statement + "export { ".len()),
        (
            "Shadow",
            "/build/Shadow",
            statement + "export { Options, ".len(),
        ),
    ];
    for (name, target, at) in selectors {
        let expected = module_runtime_range(at, name);
        let mut edges = doc.edges.iter().filter(|edge| {
            edge.kinds == [EdgeKind::References]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == unit.unit && site.range == expected)
        });
        let edge = edges.next().expect("checked local selector reference");
        assert!(edges.next().is_none());
        assert!(edge.from.as_str().ends_with("/build"));
        assert!(edge.to.as_str().ends_with(target));
    }
    let unresolved = SOURCE.find("Missing").expect("written unresolved selector");
    assert!(!doc.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::References]
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.unit == unit.unit && site.range == module_runtime_range(unresolved, "Missing")
            })
    }));
}

#[test]
fn typescript_traditional_loops_keep_their_written_local_bindings() {
    const SOURCE: &str = concat!(
        "export function scan(values: number[]) {\n",
        "    const size = values.length;\n",
        "    let total = 0;\n",
        "    for (let i = 0; i < size; i++) { total += values[i]; }\n",
        "    for (let i = size - 1; i >= 0; i--) { total += values[i]; }\n",
        "    return total;\n",
        "}\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.ts", "export { scan } from './data.js';"),
            ("data.ts", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.ts"}"#,
        )),
    )
    .expect("checked loop scopes");
    let found = result.publication();
    found.validate_facts().expect("exact loop facts");
    let [first, second] = ["i = 0", "i = size - 1"].map(|text| {
        let at = SOURCE.find(text).expect("written loop binding");
        found
            .declarations
            .iter()
            .find(|row| {
                row.source_complete
                    && row.source == text
                    && row.range == module_runtime_range(at, text)
                    && row.syntax_element_kind.as_deref() == Some("variable_declarator")
            })
            .expect("distinct loop declaration")
    });
    assert_ne!(first.symbol, second.symbol);
    assert_eq!(first.unit, second.unit);
    let uses = SOURCE
        .match_indices("values[i]")
        .map(|(start, _)| start + "values[".len())
        .collect::<Vec<_>>();
    for (target, start) in [first, second].into_iter().zip(uses) {
        assert!(found.edges.iter().any(|edge| {
            edge.to == target.symbol
                && edge.kinds == [EdgeKind::Reads]
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == target.unit && site.range == module_runtime_range(start, "i")
                })
        }));
    }
}

#[test]
fn typescript_written_arrow_arguments_keep_separate_formal_reads() {
    const SOURCE: &str = concat!(
        "export class Reader {\n",
        "  choose(items: number[]) {\n",
        "    const size = items.length;\n",
        "    const selected = items.map((value: number, index: number) => value + index + size);\n",
        "    return selected.filter((value: number, index: number) => value > index);\n",
        "  }\n",
        "}\n",
    );
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.ts"}"#;
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.ts", "export { Reader } from './data.js';"),
                ("data.ts", source),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked TypeScript arrow scopes")
    };
    let result = analyze(SOURCE);
    let found = result.publication();
    found.validate_facts().expect("exact callback facts");
    let mut seen = BTreeSet::new();
    for name in ["value", "index"] {
        let formal = format!("{name}: number");
        let declarations = SOURCE.match_indices(&formal).map(|(start, _)| start);
        let uses = SOURCE
            .match_indices(if name == "value" {
                "=> value"
            } else {
                " index"
            })
            .map(|(start, text)| start + text.len() - name.len())
            .filter(|&start| !SOURCE[start + name.len()..].starts_with(':'));
        for (declaration, read) in declarations.zip(uses) {
            let row = found
                .declarations
                .iter()
                .find(|row| {
                    row.source == name
                        && row.source_complete
                        && row.range == module_runtime_range(declaration, name)
                })
                .expect("written arrow formal");
            assert!(seen.insert(&row.symbol));
            assert!(found.edges.iter().any(|edge| {
                edge.to == row.symbol
                    && edge.kinds == [EdgeKind::Reads]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == row.unit && site.range == module_runtime_range(read, name)
                    })
            }));
        }
    }
    assert_eq!(seen.len(), 4);

    let computed = SOURCE.replace("items.map(", "items[selector](");
    let control = analyze(&computed);
    assert_eq!(
        control
            .publication()
            .declarations
            .iter()
            .filter(|row| { row.source == "value" && row.symbol.as_str().contains("/$callback") })
            .count(),
        1
    );
}

#[test]
fn returned_properties_keep_their_distinct_private_function_owners() {
    const SOURCE: &str = concat!(
        "function top(root) { return { folder: root, read: function (name) { return name; } }; }\n",
        "consume(top('/'));\n",
        "observe('one', function (input) {\n",
        "  function configure(root) {\n",
        "    return { folder: root, 'package': { entry: 'first' }, read: function (name) { return name; } };\n",
        "  }\n",
        "  consume(configure(input));\n",
        "});\n",
        "observe('two', function (input) {\n",
        "  function configure(root) {\n",
        "    return { folder: root, 'package': { entry: 'second' }, read: function (name) { return name; } };\n",
        "  }\n",
        "  consume(configure(input));\n",
        "});\n",
    );
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#;
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", source)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked returned properties")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication.validate_facts().expect("exact returned facts");
    let mut owners = publication
        .declarations
        .iter()
        .filter(|row| {
            row.source_complete
                && row.source.starts_with("function configure(")
                && row.syntax_element_kind.as_deref() == Some("function_declaration")
        })
        .collect::<Vec<_>>();
    owners.sort_by_key(|row| row.range.start);
    let [first, last] = owners.as_slice() else {
        panic!("separately placed functions")
    };
    assert_ne!(first.symbol, last.symbol);
    for owner in [first, last] {
        for (suffix, text) in [
            ("/folder", "folder: root"),
            ("/read", "read: function"),
            ("/'package'", "'package':"),
            ("/'package'/entry", "entry:"),
        ] {
            let identity = format!("{}{suffix}", owner.symbol.as_str());
            let row = publication
                .declarations
                .iter()
                .find(|row| {
                    row.symbol.as_str() == identity
                        && row.source_complete
                        && row.source.starts_with(text)
                })
                .expect("written returned property");
            assert!(owner.range.start <= row.range.start && row.range.end <= owner.range.end);
        }
    }
    assert!(publication.declarations.iter().any(|row| {
        row.symbol.as_str().ends_with("/top/folder") && row.source == "folder: root"
    }));
    let duplicate = SOURCE.replacen(
        "folder: root, 'package': { entry: 'second'",
        "folder: root, folder: input, 'package': { entry: 'second'",
        1,
    );
    let compared = analyze(&duplicate);
    assert!(
        !compared
            .publication()
            .declarations
            .iter()
            .any(|row| { row.symbol.as_str() == format!("{}/folder", last.symbol.as_str()) })
    );
}

#[test]
fn checked_commonjs_callbacks_keep_same_file_variable_reads() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"index.js"}"#;
    const ENTRY: &str = "function load() { return 1; }\nmodule.exports = load;\n";
    const CONSUMER: &str = concat!(
        "const load = require('../');\n",
        "const values = [];\n",
        "external.deliver(() => {\n",
        "    sink(values);\n",
        "    values.push(2);\n",
        "    sink(missing);\n",
        "});\n",
        "function named() { sink(values); }\n",
        "external.deliver(named);\n",
        "const assigned = () => { sink(values); };\n",
        "external.deliver(assigned);\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", ENTRY), ("test/check.js", CONSUMER)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("selected sources");
    let doc = result.publication();
    doc.validate_facts().expect("valid package facts");
    let unit = doc
        .units
        .iter()
        .find(|unit| unit.path.0 == "test/check.js")
        .expect("secondary source");
    let callbacks = doc
        .declarations
        .iter()
        .filter(|row| {
            row.unit == unit.unit
                && row.symbol.as_str().contains("$callback")
                && row.syntax_element_kind.as_deref() == Some("arrow_function")
        })
        .collect::<Vec<_>>();
    let [callback] = callbacks.as_slice() else {
        panic!("expected one declared direct callback");
    };
    assert!(callback.source_complete && callback.source.starts_with("() => {"));
    let reads = doc
        .edges
        .iter()
        .filter(|edge| edge.from == callback.symbol)
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2);
    let name = "values";
    for start in [
        CONSUMER.find("sink(values)").expect("direct argument") + "sink(".len(),
        CONSUMER.find("values.push").expect("written receiver"),
    ] {
        assert!(reads.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.to.as_str().ends_with("/values")
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == unit.unit && site.range == module_runtime_range(start, name)
                })
        }));
    }
    for owner in ["/named", "/assigned"] {
        assert!(doc.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.from.as_str().ends_with(owner)
                && edge.to.as_str().ends_with("/values")
        }));
    }
    assert!(!doc.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|span| span.unit == unit.unit)
    }));
}

#[test]
fn secondary_commonjs_callbacks_keep_separate_private_return_functions() {
    const SOURCE: &str = concat!(
        "var observe = require('observers');\n",
        "function top(input) { return { folder: input }; }\n",
        "module.exports = top;\n",
        "observe('one', function (input) {\n",
        "  function configure(root) { return { folder: root }; }\n",
        "  consume(configure(input));\n",
        "});\n",
        "observe('two', function (input) {\n",
        "  function configure(root) { return { folder: root }; }\n",
        "  consume(configure(input));\n",
        "});\n",
    );
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#;
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("index.js", "module.exports = require('./worker');"),
                ("worker.js", source),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked secondary CommonJS module")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication.validate_facts().expect("exact package facts");
    let mut owners = publication
        .declarations
        .iter()
        .filter(|row| row.source_complete && row.source.starts_with("function configure("))
        .collect::<Vec<_>>();
    owners.sort_by_key(|row| row.range.start);
    let [first, second] = owners.as_slice() else {
        panic!("both private functions have identities");
    };
    assert_ne!(first.symbol, second.symbol);
    let calls = SOURCE
        .match_indices("configure(input)")
        .map(|(start, _)| start);
    for (owner, start) in [first, second].into_iter().zip(calls) {
        assert!(publication.declarations.iter().any(|row| {
            row.symbol.as_str() == format!("{}/folder", owner.symbol.as_str())
                && row.source == "folder: root"
                && owner.range.start <= row.range.start
                && row.range.end <= owner.range.end
        }));
        assert!(publication.edges.iter().any(|edge| {
            edge.to == owner.symbol
                && edge.kinds == [EdgeKind::Calls]
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == owner.unit
                        && site.range.start == u64::try_from(start).expect("call offset")
                })
        }));
    }
    assert!(publication.declarations.iter().any(|row| {
        row.symbol.as_str().ends_with("/top/folder") && row.source == "folder: input"
    }));

    let unproved = SOURCE.replace("return { folder: root };", "return root;");
    let contrast = analyze(&unproved);
    assert!(!contrast.publication().declarations.iter().any(|row| {
        row.source.starts_with("function configure(") && row.symbol.as_str().contains("/configure")
    }));
}

#[test]
fn secondary_default_array_keeps_each_written_property_tree() {
    const SOURCE: &str = concat!(
        "const presets = [];\n",
        "export default [\n",
        "  { once: true }, { rules: { alpha: true, 'max-size': [2, { maximum: 8 }] } },\n",
        "  ...presets,\n",
        "  { rules: { beta: false, 'max-size': [3, { maximum: 12 }] } },\n",
        "];\n",
    );
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#;
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("index.mjs", "export const keep = 'value';"),
                ("config.mjs", source),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked secondary default array")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("checked property anchors");
    let mut groups = publication
        .declarations
        .iter()
        .filter(|row| {
            row.source_complete
                && row.source.starts_with("rules: {")
                && row.syntax_element_kind.as_deref() == Some("pair")
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|row| row.range.start);
    let [left, right] = groups.as_slice() else {
        panic!("separate literal property roots");
    };
    assert_ne!(left.symbol, right.symbol);
    for (root, extrema, rank) in [(left, "maximum: 8", 2), (right, "maximum: 12", 3)] {
        let scope = format!("{}/'max-size'", root.symbol.as_str());
        let child = publication
            .declarations
            .iter()
            .find(|row| {
                row.source_complete
                    && row.symbol.as_str() == scope
                    && row.source.starts_with(&format!("'max-size': [{rank}, "))
            })
            .expect("quoted child belongs to its written rules slot");
        let grandchild = publication
            .declarations
            .iter()
            .find(|row| {
                row.source_complete
                    && row.symbol.as_str() == format!("{scope}/maximum")
                    && row.source == extrema
            })
            .expect("exact option in the written list");
        assert!(root.range.start <= child.range.start && child.range.end <= root.range.end);
        assert!(
            child.range.start <= grandchild.range.start && grandchild.range.end <= child.range.end
        );
    }
    assert!(
        publication
            .declarations
            .iter()
            .any(|row| { row.symbol.as_str().ends_with("/once") && row.source == "once: true" })
    );

    let ambiguous = SOURCE.replace("{ beta: false,", "{ beta: false, beta: true,");
    let made = analyze(&ambiguous);
    assert!(
        !made
            .publication()
            .declarations
            .iter()
            .any(|row| { row.symbol.as_str() == format!("{}/beta", right.symbol.as_str()) })
    );
}

#[test]
fn typescript_class_methods_keep_repeated_direct_argument_properties() {
    const SOURCE: &str = concat!(
        "interface Command { kind: string; line: number; }\n",
        "export class Reader {\n",
        "  choose(ready: boolean) {\n",
        "    const keep = (value: Command) => value;\n",
        "    if (ready) return keep({ kind: 'ready', line: 1 } as Command);\n",
        "    return keep({ kind: 'pending', line: 2 });\n",
        "  }\n",
        "}\n",
    );
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.ts"}"#;
    let analyze = |text: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.ts", "export { Reader } from './worker.js';"),
                ("worker.ts", text),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("analyzed written method")
    };
    let result = analyze(SOURCE);
    let found = result.publication();
    found.validate_facts().expect("exact written arguments");
    let method = found
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/Reader/choose"))
        .expect("checked method");
    let mut identities = BTreeSet::new();
    for written in ["kind: 'ready'", "line: 1", "kind: 'pending'", "line: 2"] {
        let start = SOURCE.find(written).expect("property source");
        let row = found
            .declarations
            .iter()
            .find(|row| {
                row.unit == method.unit
                    && row.source_complete
                    && row.source == written
                    && row.range == module_runtime_range(start, written)
                    && row.syntax_element_kind.as_deref() == Some("pair")
            })
            .expect("written property");
        assert!(identities.insert(&row.symbol));
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&method.symbol)
        }));
    }
    assert!(found.declarations.iter().any(|row| {
        row.source == "kind: string"
            && row.syntax_element_kind.as_deref() == Some("property_signature")
            && !identities.contains(&row.symbol)
    }));

    let computed = SOURCE.replace("return keep(", "return registry[dynamic](");
    let detached = SOURCE.replace("export class Reader {", "export const Reader = {");
    for negative in [&computed, &detached] {
        assert!(
            !analyze(negative)
                .publication()
                .declarations
                .iter()
                .any(|row| {
                    row.source == "kind: 'ready'"
                        && row.syntax_element_kind.as_deref() == Some("pair")
                })
        );
    }

    let unknown = SOURCE.replace("return keep(", "return missing(");
    let control = analyze(&unknown);
    assert!(
        !control.publication().edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls] && edge.to.as_str().ends_with("/missing")
        })
    );
    assert!(control.publication().declarations.iter().any(|row| {
        row.source == "kind: 'ready'" && row.syntax_element_kind.as_deref() == Some("pair")
    }));
}

#[test]
fn inline_assigned_literal_properties_keep_their_written_local_owner() {
    const SOURCE: &str = concat!(
        "var parser;\n",
        "(function (exports) {\n",
        "  var rules, last;\n",
        "  rules = { 'alpha': ['parseAlpha'], 'beta': ['parseBeta'] };\n",
        "  function get(key) { last = key; return rules[key]; }\n",
        "  exports.get = get;\n",
        "}(parser = {}));\n",
        "exports.get = parser.get;\n",
    );
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#;
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", source)],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("assigned literal package")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("exact written properties");
    let owner = publication
        .declarations
        .iter()
        .find(|row| {
            row.syntax_element_kind.as_deref() == Some("variable_declarator")
                && row.source == "rules"
        })
        .expect("earlier local binding");
    for (key, text) in [
        ("'alpha'", "'alpha': ['parseAlpha']"),
        ("'beta'", "'beta': ['parseBeta']"),
    ] {
        let identity = format!("{}/{key}", owner.symbol.as_str());
        let property = publication
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str() == identity && row.source == text && row.source_complete
            })
            .expect("same authored property span");
        assert_eq!(
            property.range,
            module_runtime_range(SOURCE.find(text).expect("written property"), text),
        );
        assert!(publication.objects.iter().any(|row| {
            row.id.as_ref() == Some(&property.symbol)
                && row.container.as_ref() == Some(&owner.symbol)
        }));
    }
    let sibling =
        format!("(function (outside) {{ outside = {{ 'alpha': ['other'] }}; }}({{}}));\n{SOURCE}");
    let separated = analyze(&sibling);
    for (owner, source) in [
        ("/rules/'alpha'", "'alpha': ['parseAlpha']"),
        ("/beacon/'alpha'", "'alpha': ['other']"),
    ] {
        assert!(
            separated
                .publication()
                .declarations
                .iter()
                .any(|row| { row.symbol.as_str().ends_with(owner) && row.source == source })
        );
    }

    let shadow = SOURCE
        .replace("function (exports)", "function (exports, rules)")
        .replace("var rules, last;", "var last;");
    let outer = SOURCE
        .replace("var parser;", "var parser, rules;")
        .replace("var rules, last;", "var last;");
    for control in [shadow, outer] {
        let other = analyze(&control);
        assert!(other.publication().declarations.iter().any(|row| {
            row.symbol.as_str().ends_with("/'alpha'") && row.source == "'alpha': ['parseAlpha']"
        }));
        assert!(
            !other
                .publication()
                .declarations
                .iter()
                .any(|row| { row.symbol.as_str().ends_with("/rules/'alpha'") })
        );
    }
}

#[test]
fn initializer_members_stay_with_the_separately_placed_local_binding() {
    const SOURCE: &str = concat!(
        "export function make() {\n",
        "  const changes = { updated: 1, deleted: 2 };\n",
        "  return { changes };\n",
        "}\n",
    );
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#;
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.mjs", "export const keep = 0;"),
            ("body.mjs", SOURCE),
        ],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("placed initializer and shorthand");
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("checked initializer facts");
    let variable = publication
        .declarations
        .iter()
        .find(|row| {
            row.source_complete
                && row.source.starts_with("changes = {")
                && row.syntax_element_kind.as_deref() == Some("variable_declarator")
        })
        .expect("written local binding");
    let shorthand = publication
        .declarations
        .iter()
        .find(|row| {
            row.source_complete
                && row.source == "changes"
                && row.syntax_element_kind.as_deref() == Some("shorthand_property_identifier")
        })
        .expect("separately written returned property");
    assert_ne!(variable.symbol, shorthand.symbol);

    for (key, source) in [("updated", "updated: 1"), ("deleted", "deleted: 2")] {
        let row = publication
            .declarations
            .iter()
            .find(|row| {
                row.source_complete
                    && row.source == source
                    && row.symbol.as_str() == format!("{}/{key}", variable.symbol.as_str())
            })
            .expect("written member of variable initializer");
        assert_eq!(
            row.range,
            module_runtime_range(SOURCE.find(source).expect("physical member"), source),
        );
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&variable.symbol)
        }));
        assert!(!publication.declarations.iter().any(|child| {
            child.symbol.as_str() == format!("{}/{key}", shorthand.symbol.as_str())
        }));
    }
}

#[test]
fn nested_array_initializers_keep_properties_under_their_written_variable() {
    const SOURCE: &str = concat!(
        "export const values = [[{ present: 1 }], [{ present: 2 }]];\n",
        "export const generated = build([[{ present: 3 }], [{ present: 4 }]]);\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.mjs", "export const keep = 0;"),
            ("values.mjs", SOURCE),
        ],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
        )),
    )
    .expect("checked array source");
    let found = result.publication();
    found.validate_facts().expect("exact written array keys");
    let values = found
        .declarations
        .iter()
        .find(|row| {
            row.syntax_element_kind.as_deref() == Some("variable_declarator")
                && row.symbol.as_str().ends_with("/values")
        })
        .expect("physical literal initializer holder");
    let mut ids = BTreeSet::new();
    for source in ["present: 1", "present: 2"] {
        let row = found
            .declarations
            .iter()
            .find(|row| {
                row.source_complete
                    && row.source == source
                    && row.syntax_element_kind.as_deref() == Some("pair")
            })
            .expect("written property declaration");
        assert_eq!(
            row.range,
            module_runtime_range(SOURCE.find(source).expect("property byte range"), source),
        );
        assert!(ids.insert(&row.symbol));
        assert!(found.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&values.symbol)
        }));
    }
    assert!(!found.declarations.iter().any(|row| {
        row.symbol.as_str().contains("/generated/present")
            && (row.source == "present: 3" || row.source == "present: 4")
    }));
}

#[test]
fn written_prototype_alias_uses_the_checked_local_function() {
    const SOURCE: &str = concat!(
        "function Cache() {}\n",
        "function clear() { this.entries = []; }\n",
        "Cache.prototype.clear = clear;\n",
        "Cache.prototype.touch = function () { return this.entries.length; };\n",
        "module.exports = Cache;\n",
    );
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", source)],
            None,
            Some((
                "package.json",
                r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#,
            )),
        )
        .expect("checked prototype ownership")
    };
    let result = analyze(SOURCE);
    let found = result.publication();
    found.validate_facts().expect("exact method facts");
    let local = found
        .declarations
        .iter()
        .find(|row| {
            row.syntax_element_kind.as_deref() == Some("function_declaration")
                && row.source.starts_with("function clear(")
        })
        .expect("separate authored function");
    let written = "Cache.prototype.clear = clear";
    let method = found
        .declarations
        .iter()
        .find(|row| {
            row.syntax_element_kind.as_deref() == Some("assignment_expression")
                && row.symbol.as_str().ends_with("/Cache/clear")
        })
        .expect("written prototype property");
    assert_eq!(
        method.range,
        module_runtime_range(SOURCE.find(written).expect("written assignment"), written),
    );
    assert_ne!(method.symbol, local.symbol);
    assert!(found.edges.iter().any(|edge| {
        edge.from == method.symbol && edge.to == local.symbol && edge.kinds == [EdgeKind::Aliases]
    }));

    for changed in [
        SOURCE.replace(" = clear;", " = missing;"),
        SOURCE.replace(written, &format!("clear = elsewhere;\n{written}")),
        SOURCE.replace(written, &format!("Cache.prototype = elsewhere;\n{written}")),
    ] {
        let compared = analyze(&changed);
        assert!(
            !compared
                .publication()
                .declarations
                .iter()
                .any(|row| { row.symbol.as_str().ends_with("/Cache/clear") })
        );
    }
}

#[test]
fn named_function_value_keeps_checked_assigned_local_properties() {
    const SOURCE: &str = concat!(
        "var parser;\n",
        "(function (exports) {\n",
        "  function Parser() {}\n",
        "  Parser.prototype.scan = function scan() {\n",
        "    var settings;\n",
        "    settings = { 'enabled': true, 'precision': 2 };\n",
        "    return settings;\n",
        "  };\n",
        "  exports.Parser = Parser;\n",
        "}(parser = {}));\n",
        "module.exports = parser;\n",
    );
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", source)],
            None,
            Some((
                "package.json",
                r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#,
            )),
        )
        .expect("assigned local owner")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("exact assigned properties");
    let settings = publication
        .declarations
        .iter()
        .find(|row| {
            row.syntax_element_kind.as_deref() == Some("variable_declarator")
                && row.source == "settings"
        })
        .expect("written local declaration");
    for source in ["'enabled': true", "'precision': 2"] {
        let row = publication
            .declarations
            .iter()
            .find(|row| row.source == source && row.source_complete)
            .expect("complete property definition");
        assert_eq!(
            row.range,
            module_runtime_range(SOURCE.find(source).expect("pair location"), source),
        );
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol)
                && object.container.as_ref() == Some(&settings.symbol)
        }));
    }

    for refused in [
        SOURCE.replace("    var settings;\n", ""),
        SOURCE.replace("function scan() {", "function () {"),
    ] {
        let other = analyze(&refused);
        assert!(
            !other
                .publication()
                .declarations
                .iter()
                .any(|row| { row.symbol.as_str().ends_with("/settings/'enabled'") })
        );
    }
}

#[test]
fn checked_commonjs_relative_imports_keep_independently_admitted_peers() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"./index.js"}"#;
    const ENTRY: &str = "function load() { return 1; }\nmodule.exports = load;\n";
    const FIRST: &str = "const load = require('../');\nconst helper = require('./second.js');\nload();\nhelper();\n";
    const PEER: &str = "const load = require('../');\nload();\n";
    const AUTHORED: &str = "module.exports = { handle() { return 1; } };\n";
    const AUTHORED_FIRST: &str = concat!(
        "const helper = require('./second.js');\n",
        "function handle() { return helper(); }\n",
        "module.exports = { handle };\n",
    );
    const UNSEEDED: &str = "const helper = require('./second.js');\nhelper();\n";
    const SHADOWED: &str = "const require = name => name;\nrequire('./second.js');\n";
    const MISSING: &str = "const load = require('../');\nconst other = require('./missing.js');\n";

    for (first, second) in [(FIRST, PEER), (FIRST, AUTHORED), (AUTHORED_FIRST, PEER)] {
        let result = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("index.js", ENTRY),
                ("test/first.js", first),
                ("test/second.js", second),
                ("test/unseeded.js", UNSEEDED),
                ("test/shadowed.js", SHADOWED),
                ("test/missing.js", MISSING),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("selected sources");
        let publication = result.publication();
        publication.validate_facts().expect("valid package facts");
        let module_at = |path: &str| {
            let unit = publication.units.iter().find(|unit| unit.path.0 == path)?;
            let declaration = publication.declarations.iter().find(|row| {
                row.unit == unit.unit
                    && row.range.start == 0
                    && row.source_complete
                    && row.source == unit.source
                    && publication.objects.iter().any(|object| {
                        object.id.as_ref() == Some(&row.symbol)
                            && object.kinds == [rift_protocol::read::SymbolKind::Module]
                    })
            })?;
            Some((&declaration.symbol, &unit.unit))
        };
        let (from, first_unit) = module_at("test/first.js").expect("first module");
        let (to, _) = module_at("test/second.js").expect("checked peer module");
        let source = first
            .find("'./second.js'")
            .expect("written relative import");
        let written = module_runtime_range(source, "'./second.js'");
        assert_eq!(
            publication
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds == [EdgeKind::Imports]
                        && &edge.from == from
                        && &edge.to == to
                        && edge
                            .occurrence
                            .as_ref()
                            .is_some_and(|span| &span.unit == first_unit && span.range == written)
                })
                .count(),
            1
        );
        assert!(module_at("test/shadowed.js").is_none());
        assert!(module_at("test/unseeded.js").is_none() || second == AUTHORED);
        let (missing, _) = module_at("test/missing.js").expect("independent known import");
        assert_eq!(
            publication
                .edges
                .iter()
                .filter(|edge| edge.kinds == [EdgeKind::Imports] && &edge.from == missing)
                .count(),
            1
        );
    }
}

#[test]
fn assigned_shorthand_keeps_distinct_module_and_initializer_owners() {
    const SOURCE: &str = concat!(
        "let output;\n",
        "export function make() {\n",
        "    const changes = { updated: 1, deleted: 2 };\n",
        "    output = { changes };\n",
        "    return output;\n",
        "}\n",
    );
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![
                ("index.mjs", "export const keep = 1;\n"),
                ("config.mjs", source),
            ],
            None,
            Some((
                "package.json",
                r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
            )),
        )
        .expect("selected module assignment")
    };
    let result = analyze(SOURCE);
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("distinct declaration ownership");
    let written = |source: &str, kind: &str| {
        publication
            .declarations
            .iter()
            .find(|row| row.source == source && row.syntax_element_kind.as_deref() == Some(kind))
            .expect("exact authored declaration")
    };
    let output = written("output", "variable_declarator");
    let change = written(
        "changes = { updated: 1, deleted: 2 }",
        "variable_declarator",
    );
    let shorthand = written("changes", "shorthand_property_identifier");
    assert_ne!(change.symbol, shorthand.symbol);
    assert_eq!(
        shorthand.range,
        module_runtime_range(
            SOURCE.rfind("changes").expect("shorthand position"),
            "changes"
        )
    );
    for (row, owner) in [
        (shorthand, &output.symbol),
        (written("updated: 1", "pair"), &change.symbol),
        (written("deleted: 2", "pair"), &change.symbol),
    ] {
        assert!(publication.objects.iter().any(|object| {
            object.id.as_ref() == Some(&row.symbol) && object.container.as_ref() == Some(owner)
        }));
    }
    let unknown = SOURCE.replace("let output;\n", "");
    let shadowed = SOURCE.replace("    const changes", "    let output;\n    const changes");
    let competing = SOURCE.replace("    return output;", "    output = {};\n    return output;");
    for source in [unknown, shadowed, competing] {
        let other = analyze(&source);
        assert!(
            !other
                .publication()
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().ends_with("/output/changes"))
        );
    }
}

#[test]
fn retained_callback_keeps_independently_written_argument_properties() {
    const ENTRY: &str = "function helper(value) { return value; }\nmodule.exports = helper;\n";
    const SELECTED: &str = concat!(
        "var check = require('checks');\n",
        "var helper = require('../');\n",
        "check('first', function () {\n",
        "    helper('first');\n",
        "    opaque({ visible: 'direct' });\n",
        "    JSON.stringify({ visible: 'first' });\n",
        "});\n",
        "check('second', function () {\n",
        "    opaque('second');\n",
        "    JSON.stringify({ visible: 'second' });\n",
        "});\n",
    );
    let analyze = |source: &str| {
        fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", ENTRY), ("test/options.cjs", source)],
            None,
            Some((
                "package.json",
                r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#,
            )),
        )
        .expect("written callbacks with independently checked calls")
    };
    let unresolved = SELECTED.replace("helper('first')", "opaque('first')");
    let first = analyze(&unresolved);
    let second = analyze(SELECTED);
    let mut property_ids = Vec::new();
    for (analysis, calls) in [(&first, 0), (&second, 1)] {
        let publication = analysis.publication();
        publication
            .validate_facts()
            .expect("checked callback facts");
        let mut properties = Vec::new();
        for (source, ordinal) in [("visible: 'first'", 0), ("visible: 'second'", 1)] {
            let declared = publication
                .declarations
                .iter()
                .find(|row| row.source == source && row.source_complete)
                .expect("written visible property");
            assert_eq!(
                declared.range,
                module_runtime_range(SELECTED.find(source).expect("written property"), source),
            );
            let callback = publication
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&declared.symbol))
                .and_then(|object| object.container.as_ref())
                .expect("written callback owner");
            assert!(
                callback
                    .as_str()
                    .ends_with(&format!("$callback%23{ordinal}"))
            );
            properties.push((callback.clone(), declared.symbol.clone()));
        }

        let callback_calls = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds.contains(&EdgeKind::Calls) && edge.from == properties[0].0)
            .collect::<Vec<_>>();
        assert_eq!(callback_calls.len(), calls);
        assert_eq!(
            publication
                .declarations
                .iter()
                .any(|row| row.source == "visible: 'direct'"),
            calls == 0,
        );
        if let [call] = callback_calls.as_slice() {
            assert!(call.to.as_str().ends_with("/helper"));
            assert_eq!(
                call.occurrence.as_ref().expect("written Call").range,
                module_runtime_range(
                    SELECTED.find("helper('first')").expect("written call"),
                    "helper('first')",
                ),
            );
        }
        property_ids.push(properties);
    }
    assert_eq!(property_ids[0], property_ids[1]);
}

#[test]
fn checked_local_reads_keep_prior_callback_argument_properties() {
    const ENTRY: &str = "function load() { return 1; }\nmodule.exports = load;\n";
    const SELECTED: &str = concat!(
        "const load = require('../');\n",
        "const values = [];\n",
        "external.deliver(() => {\n",
        "    sink(values);\n",
        "    inspect({ flag: 'first' });\n",
        "    inspect({ flag: 'second' });\n",
        "});\n",
    );
    let unresolved = SELECTED.replace("sink(values)", "sink(opaque)");
    let with_call = SELECTED.replace("sink(values);", "sink(values);\n    load();");
    let mut incumbent = Vec::new();
    for (source, has_read, has_call) in [
        (unresolved.as_str(), false, false),
        (SELECTED, true, false),
        (with_call.as_str(), true, true),
    ] {
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            vec![("index.js", ENTRY), ("test/check.js", source)],
            None,
            Some((
                "package.json",
                r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#,
            )),
        )
        .expect("written callback and local variable");
        let publication = analysis.publication();
        publication.validate_facts().expect("exact callback facts");
        let callback = publication
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with("$callback%230"))
            .expect("written callback");
        let edge_exists = |kind, target: &str, site: &str, offset| {
            publication.edges.iter().any(|edge| {
                edge.from == callback.symbol
                    && edge.kinds == [kind]
                    && edge.to.as_str().ends_with(target)
                    && edge.occurrence.as_ref().is_some_and(|at| {
                        at.unit == callback.unit && at.range == module_runtime_range(offset, site)
                    })
            })
        };
        let value = source.find("sink(").expect("consumer") + "sink(".len();
        assert_eq!(
            edge_exists(EdgeKind::Reads, "/values", "values", value),
            has_read
        );
        if has_call {
            let call = source.find("load();").expect("checked local call");
            assert!(edge_exists(EdgeKind::Calls, "/load", "load()", call));
        } else {
            assert!(!publication.edges.iter().any(|edge| {
                edge.from == callback.symbol && edge.kinds.contains(&EdgeKind::Calls)
            }));
        }
        let mut ids = Vec::new();
        for written in ["flag: 'first'", "flag: 'second'"] {
            let field = publication
                .declarations
                .iter()
                .find(|row| row.source == written);
            if has_call {
                assert!(field.is_none());
                continue;
            }
            let field = field.expect("prior written property");
            assert!(field.source_complete);
            assert_eq!(field.unit, callback.unit);
            assert_eq!(
                field.range,
                module_runtime_range(source.find(written).expect("written property"), written),
            );
            let container = publication
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&field.symbol))
                .and_then(|object| object.container.as_ref());
            assert_eq!(container, Some(&callback.symbol));
            ids.push(field.symbol.clone());
        }
        if !has_read {
            incumbent = ids;
        } else if !has_call {
            assert_eq!(ids, incumbent);
        }
    }
}

#[test]
fn checked_callback_arguments_keep_distinct_written_quote_and_nested_pairs() {
    const ENTRY: &str = "module.exports = function helper() {};\n";
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","main":"./index.js"}"#;
    const SOURCE: &str = concat!(
        "var check = require('checks');\n",
        "check('first', function () {\n",
        "    inspect({ ordinary: 1, 'detail': { nested: 1 }, [dynamic]: 1 });\n",
        "    inspect({ ordinary: 2, \"ambiguous\": 1, 'ambiguous': 2 });\n",
        "});\n",
        "check('second', function () {\n",
        "    inspect({ ordinary: 3, 'detail': { nested: 2 } });\n",
        "});\n",
    );
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", ENTRY), ("test/options.cjs", SOURCE)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("quoted arguments in selected callbacks");
    let graph = result.publication();
    graph
        .validate_facts()
        .expect("exact authored argument properties");
    let written = |source: &str| {
        let start = SOURCE.find(source).expect("written source");
        graph
            .declarations
            .iter()
            .find(|row| {
                row.source == source
                    && row.source_complete
                    && row.range == module_runtime_range(start, source)
            })
            .expect("exact written property retained")
    };
    let parent = |id: &rift_protocol::read::SymbolId| {
        graph
            .objects
            .iter()
            .find(|row| row.id.as_ref() == Some(id))
            .and_then(|row| row.container.as_ref())
            .expect("property owner")
    };
    for (ordinal, quote, nested) in [
        (0, "'detail': { nested: 1 }", "nested: 1"),
        (1, "'detail': { nested: 2 }", "nested: 2"),
    ] {
        let quoted = written(quote);
        let child = written(nested);
        assert!(
            parent(&quoted.symbol)
                .as_str()
                .ends_with(&format!("$callback%23{ordinal}"))
        );
        assert_eq!(parent(&child.symbol), &quoted.symbol);
        assert_eq!(
            parent(
                &written(if ordinal == 0 {
                    "ordinary: 1"
                } else {
                    "ordinary: 3"
                })
                .symbol
            ),
            parent(&quoted.symbol)
        );
    }
    for declined in graph.declarations.iter().filter(|row| {
        matches!(
            row.source.as_str(),
            "\"ambiguous\": 1" | "'ambiguous': 2" | "[dynamic]: 1"
        )
    }) {
        let object = graph
            .objects
            .iter()
            .find(|row| row.id.as_ref() == Some(&declined.symbol))
            .expect("existing unscoped property");
        assert!(object.container.is_none());
    }
    assert!(
        !graph
            .edges
            .iter()
            .any(|edge| edge.kinds == [EdgeKind::Calls] && edge.to.as_str().ends_with("/inspect"))
    );
}

#[test]
fn ecmascript_class_members_keep_distinct_static_and_instance_scopes() {
    const BODY: &str = concat!(
        "function classStep() { return 1; }\n",
        "function objectStep() { return 2; }\n",
        "export class Codec {\n",
        "  static value = 1;\n",
        "  value = 2;\n",
        "  static get state() { return 1; }\n",
        "  get state() { return 2; }\n",
        "  set state(value) {}\n",
        "  static encode() { return classStep(); }\n",
        "  encode() { return objectStep(); }\n",
        "  static [unknown()]() {}\n",
        "  [unknown()]() {}\n",
        "}\n",
        "export function classUse() { return Codec.encode(); }\n",
        "export function objectUse() { return new Codec().encode(); }\n",
        "export function classValue() { return Codec.value; }\n",
        "export function objectValue() { return new Codec().value; }\n",
        "export function replaceAlias() {\n",
        "  let alias = Codec;\n",
        "  alias = unproved;\n",
        "  return alias.encode();\n",
        "}\n",
    );
    for (language, entry, inner, selector) in [
        (
            ShippedLanguage::JavaScript,
            "index.mjs",
            "codec.mjs",
            "main",
        ),
        (
            ShippedLanguage::TypeScript,
            "index.mts",
            "codec.mts",
            "types",
        ),
    ] {
        let metadata = format!(
            r#"{{"name":"beacon","version":"1.0.0","type":"module","{selector}":"./{entry}"}}"#,
        );
        let typed = BODY.replacen(
            "  static encode() {",
            "  static encode(): number;\n  static encode() {",
            1,
        );
        let (body, static_declarations) = if language == ShippedLanguage::TypeScript {
            (typed.as_str(), 2)
        } else {
            (BODY, 1)
        };
        for non_entry in [false, true] {
            let source_path = if non_entry { inner } else { entry };
            let mut files = vec![(source_path, body)];
            if non_entry {
                files.push((entry, "export const ready = true;"));
            }
            let analysis = fixture::package_result_with_metadata(
                language,
                files,
                None,
                Some(("package.json", &metadata)),
            )
            .expect("checked authored class package");
            assert_ecmascript_class_member_scopes(
                analysis.publication(),
                source_path,
                static_declarations,
            );
        }
    }
}

fn assert_ecmascript_class_member_scopes(
    found: &PackagePublication,
    source_path: &str,
    static_declarations: usize,
) {
    found.validate_facts().expect("valid class publication");
    let rows = |name, is_static| {
        found
            .objects
            .iter()
            .filter(|row| {
                row.name == name
                    && row.kinds.contains(&SymbolKind::Static) == is_static
                    && row.id.is_some()
                    && row
                        .container
                        .as_ref()
                        .is_some_and(|id| id.as_str().ends_with("/Codec"))
            })
            .collect::<Vec<_>>()
    };
    let identity = |name, is_static, declaration_count| {
        let objects = rows(name, is_static);
        let [object] = objects.as_slice() else {
            panic!("one placed class member {source_path} {name} {is_static}: {objects:?}");
        };
        let id = object.id.as_ref().expect("class member identity");
        assert_eq!(
            found
                .declarations
                .iter()
                .filter(|row| &row.symbol == id)
                .count(),
            declaration_count,
        );
        id
    };
    let static_method = identity("encode", true, static_declarations);
    let instance_method = identity("encode", false, 1);
    let static_value = identity("value", true, 1);
    let instance_value = identity("value", false, 1);
    assert_ne!(static_method, instance_method);
    assert_ne!(static_value, instance_value);
    assert_ne!(identity("state", true, 1), identity("state", false, 2));
    for (from, to, kind) in [
        ("/classUse", static_method, EdgeKind::Calls),
        ("/objectUse", instance_method, EdgeKind::Calls),
        ("/classValue", static_value, EdgeKind::Reads),
        ("/objectValue", instance_value, EdgeKind::Reads),
    ] {
        assert!(
            found.edges.iter().any(|edge| {
                edge.from.as_str().ends_with(from) && &edge.to == to && edge.kinds == [kind]
            }),
            "{source_path} keeps {from} -> {to:?}"
        );
    }
    for (from, to) in [
        (static_method, "/classStep"),
        (instance_method, "/objectStep"),
    ] {
        assert!(found.edges.iter().any(|edge| {
            &edge.from == from && edge.to.as_str().ends_with(to) && edge.kinds == [EdgeKind::Calls]
        }));
    }
    assert!(found.edges.iter().all(|edge| {
        !edge.from.as_str().ends_with("/replaceAlias")
            || ![static_method, instance_method].contains(&&edge.to)
    }));
    assert!(rows("[unknown()]", true).len() + rows("[unknown()]", false).len() <= 1);
}

#[test]
fn non_entry_typescript_class_keeps_overloads_of_each_method_owner() {
    const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","exports":{".":{"types":"./index.d.ts"},"./*":"./*.js"}}"#;
    const INSTANCE: &str = "  write(value: number): void;\n  write(value: string): void;\n";
    const STATIC: &str =
        "  static write(value: number): void;\n  static write(value: string): void;\n";
    const DISTINCT: &str = "  build(value: number): void;\n  build(value: string): void;\n";
    const FILE: &str = "writer.d.ts";
    for [first, second] in [[INSTANCE, STATIC], [STATIC, INSTANCE], [INSTANCE, DISTINCT]] {
        let source = format!(
            "export declare class Writer {{\n{first}{second}  solo(value: boolean): void;\n}}\n"
        );
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.d.ts", "export { Writer } from './writer.js';"),
                (FILE, source.as_str()),
            ],
            None,
            Some(("package.json", MANIFEST)),
        )
        .expect("checked declaration package");
        let found = analysis.publication();
        found
            .validate_facts()
            .expect("valid declaration publication");
        let unit = found
            .units
            .iter()
            .find(|row| row.path.0 == FILE)
            .expect("written declaration unit");
        let method =
            |name, is_static, count| {
                let rows: Vec<_> = found
                    .objects
                    .iter()
                    .filter(|row| {
                        row.name == name
                            && row.kinds.contains(&SymbolKind::Static) == is_static
                            && row
                                .container
                                .as_ref()
                                .is_some_and(|id| id.as_str().ends_with("/Writer"))
                            && row.id.is_some()
                    })
                    .collect();
                let [object] = rows.as_slice() else {
                    panic!("one checked method owner for {name} static={is_static}: {rows:?}");
                };
                let id = object.id.as_ref().expect("member identity");
                let facets: Vec<_> = found
                    .declarations
                    .iter()
                    .filter(|row| &row.symbol == id)
                    .collect();
                assert_eq!(facets.len(), count, "{name} static={is_static}");
                assert!(
                    facets.iter().all(|row| row.unit == unit.unit
                        && row.source.starts_with("static ") == is_static)
                );
                id
            };
        let instance = method("write", false, 2);
        let independent = if second == DISTINCT {
            method("build", false, 2)
        } else {
            method("write", true, 2)
        };
        assert_ne!(instance, independent);
        method("solo", false, 1);
    }
}

#[test]
fn checked_typescript_source_reexport_retains_its_module_and_specifier() {
    const META: &str = r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./dist/index.js","types":"./dist/index.d.ts"}"#;
    const FACADE: &str = "export { ready } from './worker.js';\n";
    const USE: &str = "import { ready } from '../index.js';\nexport const observed = ready;\n";
    fn collect(facade: &str, caller: &str) -> PackageAnalysis {
        internal_modules::analyze(
            META,
            &[
                ("dist/index.js", "export const ready = 1;\n"),
                ("dist/index.d.ts", "export declare const ready: number;\n"),
                ("src/index.ts", facade),
                ("src/worker.ts", "export const ready = 1;\n"),
                ("src/tests/check.ts", caller),
            ],
        )
    }
    let analysis = collect(FACADE, USE);
    let graph = analysis.publication();
    graph
        .validate_facts()
        .expect("checked selected source module");
    let source_unit = graph
        .units
        .iter()
        .find(|unit| unit.path.0 == "src/index.ts")
        .expect("selected complete unit");
    let module = graph
        .declarations
        .iter()
        .find(|row| {
            row.unit == source_unit.unit
                && row.range.start == 0
                && row.source == FACADE
                && graph.objects.iter().any(|object| {
                    object.id.as_ref() == Some(&row.symbol)
                        && object.kinds.contains(&SymbolKind::Module)
                })
        })
        .expect("whole reexport module");
    let caller = graph
        .units
        .iter()
        .find(|unit| unit.path.0 == "src/tests/check.ts")
        .expect("selected import unit");
    let specifier = "'../index.js'";
    let begin = USE.find(specifier).expect("written specifier");
    let end = begin + specifier.len();
    assert!(graph.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Imports]
            && edge.to == module.symbol
            && edge.occurrence.as_ref().is_some_and(|site| {
                site.unit == caller.unit
                    && usize::try_from(site.range.start) == Ok(begin)
                    && usize::try_from(site.range.end) == Ok(end)
            })
    }));
    assert!(
        graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads] && edge.to.0.contains("worker.ts/ready")
        })
    );

    let missing_target = USE.replace("../index.js", "../absent.js");
    let missing = collect(FACADE, &missing_target);
    assert!(
        !missing
            .publication()
            .edges
            .iter()
            .any(|edge| { edge.kinds == [EdgeKind::Imports] && edge.to == module.symbol })
    );
    for source in ["0;\n", "import './worker.js';\n"] {
        let without_reexport = collect(source, USE);
        assert!(
            !without_reexport
                .publication()
                .declarations
                .iter()
                .any(|row| row.symbol == module.symbol)
        );
    }
}
