//! Publication of edges from checked file modules.

use super::*;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::read::{EdgeDerivation, EdgeProvider, SourceLocation};

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
        assert_eq!(written_imports.len(), 1, "{written_imports:?}");
        assert_eq!(written_imports[0].to.as_str(), target);
        let binding = written_imports[0]
            .occurrence
            .as_ref()
            .expect("written import");
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
        assert!(written_imports.is_empty());
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
    let available_before = publication
        .edges
        .iter()
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
