//! Checked parameters of complete named signatures in a declaration file.

use super::*;

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
