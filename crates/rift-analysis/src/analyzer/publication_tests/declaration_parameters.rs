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
