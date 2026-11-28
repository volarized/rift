//! Checked parameters of complete named signatures in a declaration file.

use super::*;
use rift_protocol::read::{EdgeDerivation, EdgeProvider};

const ORDINARY: &str = concat!(
    "declare function prepare(item: string, optional?: number): string;\n",
    "declare class Shelf { open(entry: string): string; }\n",
    "export { prepare, Shelf };\n",
);
const CONTROLS: &str = concat!(
    "export type Source = import('./leaf.js').Source;\n",
    "export type Reader = (key: string) => string;\n",
    "export declare function predicate(unknownItem: unknown): unknownItem is import('./leaf.js').Source;\n",
    "export declare function lookup(unknownKey: import('./leaf.js').Source['mode']): string;\n",
    "export declare function duplicate(unique: string): string;\n",
    "export declare function duplicate(unique: number): number;\n",
);

#[test]
fn checked_named_declaration_parameters_survive_a_separate_import_type() {
    for (path, types, expected) in [
        ("lib/typed.d.ts", "./index.d.ts", 3),
        ("lib/typed.d.ts", "./lib/typed.d.ts", 3),
        ("lib/typed.ts", "./index.d.ts", 0),
    ] {
        let source = format!("{ORDINARY}{CONTROLS}");
        let manifest =
            format!(r#"{{"name":"beacon","version":"1.0.0","type":"module","types":"{types}"}}"#);
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.d.ts", "export * from './lib/typed.js';\n"),
                (
                    "lib/leaf.d.ts",
                    "export interface Source { mode: string; }\n",
                ),
                (path, &source),
            ],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("selected declaration package");
        let (publication, files, _, _) = analysis.into_parts();
        publication.validate_facts().expect("checked publication");
        let held = files
            .iter()
            .find(|file| file.file().path().as_str() == path)
            .expect("authored source");
        assert!(held.file().syntax().has_errors());
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("source unit");
        assert!(unit.source_complete && unit.source == source);
        let parameters = publication
            .declarations
            .iter()
            .filter(|row| row.unit == unit.unit)
            .filter_map(|row| {
                let object = publication
                    .objects
                    .iter()
                    .find(|object| object.id.as_ref() == Some(&row.symbol))?;
                object.kinds.contains(&SymbolKind::Parameter).then_some(row)
            })
            .collect::<Vec<_>>();
        assert_eq!(parameters.len(), expected, "{path}");
        if expected != 0 {
            for (owner, name) in [
                ("prepare", "item"),
                ("prepare", "optional"),
                ("Shelf/open", "entry"),
            ] {
                let start =
                    u64::try_from(source.find(name).expect("written identifier")).expect("offset");
                let end = start + u64::try_from(name.len()).expect("length");
                assert!(parameters.iter().any(|row| {
                    row.symbol.as_str().ends_with(&format!("/{owner}/{name}"))
                        && row.range == (TextRange { start, end })
                }));
            }
        }
    }
}

const SELECTORS: &str = concat!(
    "// é\n",
    "declare function prepare(item: string): string;\n",
    "declare class Shelf { open(entry: string): string; }\n",
    "interface LocalType { mode: string; }\n",
    "export { prepare as outward, Shelf };\n",
    "export type { LocalType };\n",
    "export type Source = import('./leaf.js').Source;\n",
    "declare function uncertain(value: import('./leaf.js').Source['mode']): string;\n",
    "declare const clash: string; declare const clash: number;\n",
    "export { clash, Missing, uncertain };\n",
    "declare namespace Inner { export interface Inside { value: string; } }\n",
    "export { Inside };\n",
    "export { Source as Broken from './leaf.js';\n",
    "export { External } from './leaf.js';\n",
);

#[test]
fn checked_local_export_selectors_survive_a_separate_import_type() {
    for (path, types, expected) in [
        ("lib/typed.d.ts", "./index.d.ts", 3),
        ("lib/typed.d.ts", "./lib/typed.d.ts", 3),
        ("lib/typed.ts", "./index.d.ts", 0),
    ] {
        let manifest =
            format!(r#"{{"name":"beacon","version":"1.0.0","type":"module","types":"{types}"}}"#);
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::TypeScript,
            vec![
                ("index.d.ts", "export * from './lib/typed.js';\n"),
                ("lib/leaf.d.ts", "export interface Source { mode: string; } export interface External { active: boolean; }\n"),
                (path, SELECTORS),
            ],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("selected declaration package");
        let (publication, files, semantics, _) = analysis.into_parts();
        publication.validate_facts().expect("checked publication");
        let held = files
            .iter()
            .find(|file| file.file().path().as_str() == path)
            .expect("authored source");
        assert!(held.file().syntax().has_errors());
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("source unit");
        assert!(unit.source_complete && unit.source == SELECTORS);
        let references = publication
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::References]
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.unit == unit.unit)
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(references.len(), expected, "{path}");
        if expected == 0 {
            continue;
        }
        let module = publication
            .declarations
            .iter()
            .find(|declaration| {
                declaration.unit == unit.unit
                    && publication.objects.iter().any(|object| {
                        object.id.as_ref() == Some(&declaration.symbol)
                            && object.kinds.contains(&SymbolKind::Module)
                    })
            })
            .expect("physically placed module");
        for (statement, name) in [
            ("prepare as outward", "prepare"),
            ("Shelf };", "Shelf"),
            ("LocalType };", "LocalType"),
        ] {
            let start = u64::try_from(SELECTORS.find(statement).expect("written selector"))
                .expect("byte offset");
            let end = start + u64::try_from(name.len()).expect("identifier length");
            let target = format!("{}/{name}", module.symbol.as_str());
            assert!(
                references.iter().any(|edge| {
                    edge.from == module.symbol
                        && edge.to.as_str() == target
                        && edge.provider == EdgeProvider::Static
                        && edge.derivation == EdgeDerivation::Resolution
                        && edge
                            .occurrence
                            .as_ref()
                            .is_some_and(|site| site.range == TextRange { start, end })
                }),
                "{path}: {target}"
            );
        }
        assert!(
            publication
                .edges
                .iter()
                .any(|edge| edge.kinds == [EdgeKind::Aliases]
                    && edge.from.as_str().ends_with("/outward")
                    && edge.to.as_str() == format!("{}/prepare", module.symbol.as_str()))
        );

        assert_local_export_bounds(&publication, &semantics, &unit.unit, &references);
    }
}

fn assert_local_export_bounds(
    publication: &PackagePublication,
    semantics: &crate::WorkspaceSemantics,
    unit: &rift_protocol::read::SourceUnitId,
    references: &[rift_protocol::index::StoredEdge],
) {
    let prior = publication
        .edges
        .iter()
        .filter(|edge| !references.contains(edge))
        .cloned()
        .collect::<Vec<_>>();
    assert!(!prior.is_empty());
    for spare in [0, 1, references.len()] {
        let mut records = Records {
            objects: publication.objects.clone(),
            declarations: publication.declarations.clone(),
            units: publication.units.clone(),
            edges: prior.clone(),
            ..Records::default()
        };
        records.retain_incomplete_local_export_references(semantics, prior.len() + spare);
        assert_eq!(&records.edges[..prior.len()], prior.as_slice());
        assert_eq!(records.edges.len(), prior.len() + spare);
        assert!(
            records.edges[prior.len()..]
                .iter()
                .all(|edge| references.contains(edge))
        );
    }
    let mut records = Records {
        objects: publication.objects.clone(),
        declarations: publication.declarations.clone(),
        units: publication.units.clone(),
        edges: prior.clone(),
        ..Records::default()
    };
    for retained in &mut records.units {
        if retained.unit == *unit {
            retained.source_complete = false;
        }
    }
    records.retain_incomplete_local_export_references(semantics, prior.len() + references.len());
    assert_eq!(records.edges, prior);
}
#[test]
fn mapped_type_parameters_keep_their_exact_alias_and_distinct_bodies() {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#;
    const SOURCE: &str = concat!(
        "export type Copy<T> = { [Key in keyof T]: { [Key in keyof T]: T[Key] } };\n",
        "export type Remap<T> = { [Key in keyof T as Exclude<Key, 'omitted'>]?: T[Key] };\n",
        "export type Ordinary<T, Key extends keyof T> = T[Key];\n",
        "export interface Lookup { [key: string]: number }\n",
    );
    for (path, export) in [
        ("lib/copy.d.ts", "export * from './lib/copy.js';"),
        ("lib/copy.d.cts", "export * from './lib/copy.cjs';"),
    ] {
        for competing in [false, true] {
            let source = if competing {
                format!("{SOURCE}export type Copy<T> = T;\n")
            } else {
                SOURCE.to_owned()
            };
            let analysis = fixture::package_result_with_metadata(
                ShippedLanguage::TypeScript,
                vec![("index.d.ts", export), (path, &source)],
                None,
                Some(("package.json", MANIFEST)),
            )
            .expect("selected declarations");
            let publication = analysis.publication();
            publication.validate_facts().expect("valid declarations");
            let unit = publication
                .units
                .iter()
                .find(|unit| unit.path.0 == path)
                .expect("written source");
            assert!(unit.source_complete && unit.source == source);
            let rows = publication
                .declarations
                .iter()
                .filter(|row| row.unit == unit.unit);
            for written in ["Key extends", "[key:"] {
                let start = source.find(written).expect("written incumbent")
                    + usize::from(written.starts_with('['));
                let start = u64::try_from(start).expect("incumbent offset");
                assert!(
                    rows.clone()
                        .any(|row| row.range.start == start && row.range.end == start + 3)
                );
            }
            let mut matched = std::collections::BTreeSet::new();
            for (index, (offset, _)) in source.match_indices("Key in").enumerate() {
                let start = u64::try_from(offset).expect("mapped offset");
                let row = rows
                    .clone()
                    .find(|row| row.range.start == start && row.range.end == start + 3);
                if competing && index < 2 {
                    assert!(row.is_none());
                    continue;
                }
                let row = row.expect("written mapped parameter");
                assert!(row.source_complete && row.source == "Key");
                let object = publication
                    .objects
                    .iter()
                    .find(|object| object.id.as_ref() == Some(&row.symbol))
                    .expect("mapped parameter object");
                assert!(object.kinds.contains(&SymbolKind::TypeParameter));
                let owner = object.container.as_ref().expect("type alias owner");
                assert!(
                    owner
                        .as_str()
                        .ends_with(if index < 2 { "/Copy" } else { "/Remap" })
                );
                let alias = rows
                    .clone()
                    .find(|candidate| &candidate.symbol == owner)
                    .expect("written alias declaration");
                assert_eq!(row.public, alias.public);
                assert!(
                    publication
                        .edges
                        .iter()
                        .all(|edge| { edge.to != row.symbol || edge.occurrence.is_none() })
                );
                assert!(matched.insert(row.symbol.as_str()));
            }
            assert_eq!(matched.len(), if competing { 1 } else { 3 });
        }
    }
}

#[test]
fn mapped_type_parameters_use_only_remaining_declaration_capacity() {
    let mut identity = fixture::identity();
    identity.manager = "npm".to_owned();
    identity.registry = "registry.npmjs.org".to_owned();
    let owner = identity.owner().expect("package owner");
    let origin = fixture::origin(&identity);
    let language = ShippedLanguage::TypeScript.language();
    let path = ProjectPath::new("index.d.ts").expect("entry path");
    let manifest = ProjectPath::new("package.json").expect("manifest path");
    let files = [PackageSource::new(
        &path,
        "export type Copy<Original> = { [Key in keyof Original]: Original[Key] };\n",
    )];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","types":"./index.d.ts"}"#,
    )];
    let collect = |declarations| {
        let mut limits = ExactPackageLimits::new(2, 4096);
        if let Some(declarations) = declarations {
            limits = limits
                .with_declarations(declarations)
                .expect("declaration limit");
        }
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
            .expect("complete source")
            .with_framework_context(&metadata, &[])
            .expect("npm manifest");
        PackageAnalyzer::analyze(input, 1)
            .expect("selected declarations")
            .into_parts()
            .0
    };
    let full = collect(None);
    let remaining = full
        .declarations
        .iter()
        .filter(|row| row.source != "Key")
        .collect::<Vec<_>>();
    assert_eq!(full.declarations.len(), remaining.len() + 1);
    let limit = u32::try_from(remaining.len()).expect("earlier declarations");
    let bounded = collect(Some(limit));
    assert_eq!(bounded.declarations.iter().collect::<Vec<_>>(), remaining);
}

#[test]
fn callable_type_rest_parameters_keep_their_written_signatures() {
    const REST_ONLY: &str = "    (...early: number[]): Reader;\n";
    const SOURCE: &str = concat!(
        "export type Reader = (input: string, ...restArgs: number[]) => number;\n",
        "export interface Route {\n",
        "    (...early: number[]): Reader;\n",
        "    (query: string, ...invokeArgs: string[]): Reader;\n",
        "    new (name: string, ...ctorArgs: number[]): Reader;\n",
        "}\n",
        "export type Factory = new (route: Reader, ...values: string[]) => Route;\n",
    );
    for (path, secondary) in [("index.d.ts", false), ("lib/worker.d.ts", true)] {
        let collect = |text: &str| {
            let manifest =
                r#"{"name":"beacon","version":"1.0.0","type":"module","types":"index.d.ts"}"#;
            let mut files = vec![(path, text)];
            if secondary {
                files.push(("index.d.ts", "export * from './lib/worker.js';"));
            }
            fixture::package_result_with_metadata(
                ShippedLanguage::TypeScript,
                files,
                None,
                Some(("package.json", manifest)),
            )
            .expect("checked declaration source")
            .into_parts()
            .0
        };
        let full = collect(SOURCE);
        let without = collect(&SOURCE.replace(REST_ONLY, ""));
        let identifier = |publication: &PackagePublication, name: &str| {
            let row = publication
                .declarations
                .iter()
                .find(|row| {
                    row.source == name && row.symbol.as_str().ends_with(&format!("/{name}"))
                })
                .expect("written parameter");
            assert_eq!(row.syntax_element_kind.as_deref(), Some("identifier"));
            let object = publication
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&row.symbol))
                .expect("parameter object");
            assert!(object.kinds.contains(&SymbolKind::Parameter));
            row.symbol.clone()
        };
        for (plain, rest) in [
            ("input", "restArgs"),
            ("query", "invokeArgs"),
            ("name", "ctorArgs"),
            ("route", "values"),
        ] {
            let parameter = identifier(&full, plain);
            assert_eq!(parameter, identifier(&without, plain));
            let additional = identifier(&full, rest);
            assert_eq!(
                parameter
                    .as_str()
                    .rsplit_once('/')
                    .expect("written signature")
                    .0,
                additional
                    .as_str()
                    .rsplit_once('/')
                    .expect("written signature")
                    .0
            );
            assert!(
                full.edges
                    .iter()
                    .all(|edge| edge.to != additional || edge.kinds != [EdgeKind::Reads])
            );
        }
        let rest_only = identifier(&full, "early");
        let ordinary = identifier(&full, "query");
        assert_ne!(
            rest_only
                .as_str()
                .rsplit_once('/')
                .expect("rest-only signature")
                .0,
            ordinary
                .as_str()
                .rsplit_once('/')
                .expect("ordinary signature")
                .0
        );
    }
}

#[test]
fn callable_type_object_patterns_keep_their_written_bindings() {
    const PATTERN_ONLY: &str = "    ({ label, limit: amount }: Options): Reader;\n";
    const SOURCE: &str = concat!(
        "export interface Options { label: string; limit: number; }\n",
        "export type Reader = ({ label, limit: amount }: Options, ...items: number[]) => typeof label;\n",
        "export interface Router {\n",
        "    ({ label, limit: amount }: Options): Reader;\n",
        "    (...rates: number[]): Reader;\n",
        "    (key: string, { label }: Options): Reader;\n",
        "    new ({ limit }: Options): Reader;\n",
        "}\n",
        "export type Factory = new ({ label }: Options) => Reader;\n",
        "export declare const optional: ({ label, limit }?: Options) => Reader;\n",
        "export type Refused = ({ label = 'default', limit: { amount }, [lookup]: skipped }: Options) => string;\n",
    );
    for path in ["index.d.ts", "lib/worker.d.ts"] {
        let full = callable_object_pattern_publication(path, SOURCE);
        let without = callable_object_pattern_publication(path, &SOURCE.replace(PATTERN_ONLY, ""));
        let parameter = |publication: &PackagePublication, owner: &str, name: &str| {
            publication.declarations.iter().find_map(|row| {
                let id = row.symbol.as_str();
                (row.source == name
                    && id.contains(&format!("/{owner}/$signature"))
                    && id.ends_with(&format!("/{name}")))
                .then(|| row.symbol.clone())
            })
        };
        for (owner, name) in [("Reader", "items"), ("Router", "key"), ("Router", "rates")] {
            assert_eq!(
                parameter(&full, owner, name),
                parameter(&without, owner, name)
            );
            assert!(parameter(&full, owner, name).is_some());
        }
        let identifier = |owner: &str, name: &str, syntax: &str, tail: &str| {
            let row = full
                .declarations
                .iter()
                .find(|row| {
                    row.source == name
                        && row.source_complete
                        && row.syntax_element_kind.as_deref() == Some(syntax)
                        && row
                            .symbol
                            .as_str()
                            .contains(&format!("/{owner}/$signature"))
                        && row.symbol.as_str().ends_with(tail)
                })
                .expect("authored binding");
            assert!(full.objects.iter().any(|object| {
                object.id.as_ref() == Some(&row.symbol)
                    && object.kinds.contains(&SymbolKind::Parameter)
            }));
            assert!(
                !full
                    .edges
                    .iter()
                    .any(|edge| { edge.to == row.symbol && edge.kinds == [EdgeKind::Reads] })
            );
            row.symbol.clone()
        };
        let name = identifier(
            "Reader",
            "label",
            "shorthand_property_identifier_pattern",
            "/label",
        );
        let rest = parameter(&full, "Reader", "items").expect("written rest formal");
        assert_eq!(
            name.as_str().rsplit_once('/').map(|parts| parts.0),
            rest.as_str().rsplit_once('/').map(|parts| parts.0)
        );
        identifier("Reader", "amount", "identifier", "/amount");
        identifier(
            "Router",
            "limit",
            "shorthand_property_identifier_pattern",
            "/limit",
        );
        identifier(
            "Factory",
            "label",
            "shorthand_property_identifier_pattern",
            "/label",
        );
        for (name, tail) in [("label", "/label"), ("limit", "/limit")] {
            identifier(
                "optional",
                name,
                "shorthand_property_identifier_pattern",
                tail,
            );
        }
        assert!(full.declarations.iter().any(|row| {
            row.source == "label: string" && row.symbol.as_str().ends_with("/Options/label")
        }));
        for withheld in ["label", "amount", "skipped"] {
            assert!(parameter(&full, "Refused", withheld).is_none());
        }
    }
}

fn callable_object_pattern_publication(path: &str, source: &str) -> PackagePublication {
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"module","types":"index.d.ts"}"#;
    let mut files = vec![(path, source)];
    if path != "index.d.ts" {
        files.push(("index.d.ts", "export * from './lib/worker.js';"));
    }
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        files,
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("checked declaration source");
    result
        .publication()
        .validate_facts()
        .expect("valid authored declarations");
    result.into_parts().0
}
