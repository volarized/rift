//! Publication of qualified inline import types from exact declaration source.

use super::*;

const MANIFEST: &str =
    r#"{"name":"beacon","version":"1.0.0","type":"module","types":"./index.d.ts"}"#;
const ROOT: &str = "export * from './lib/consumer.js';\n";
const CONTRACT: &str = "export interface Shape { width: number; }\n";
const INLINE: &str = concat!(
    "// é\n",
    "interface Shape { local: boolean; }\n",
    "export declare function read(value: import('./contract.js').Shape): number;\n",
);
const NAMED: &str = concat!(
    "import type { Shape as Input } from './contract.js';\n",
    "export declare function read(value: Input | null): number;\n",
);

fn analyze(source: &str, contract: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![
            ("index.d.ts", ROOT),
            ("lib/consumer.d.ts", source),
            ("lib/contract.d.ts", contract),
            ("lib/contract.js", "export const Shape = 1;\n"),
        ],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("selected type package")
}

#[test]
fn inline_import_type_qualifiers_use_checked_relative_declarations() {
    let missing = INLINE.replace("./contract.js", "./missing.js");
    let wrong_space =
        CONTRACT.replace("interface Shape { width: number; }", "const Shape: number;");
    let nested = INLINE.replace("'./contract.js').Shape", "'./contract.js').Shape.Width");
    let unplaced_owner = INLINE.replace(").Shape): number", ").Shape | null): number");
    let malformed = INLINE.replace(").Shape", ", undefined).Shape");
    let competing = format!("{INLINE}export declare function read(value: number): number;\n");
    let type_query = INLINE.replace("import('./contract.js')", "typeof import('./contract.js')");
    let quoted = INLINE.replace(
        "import('./contract.js').Shape",
        "\"import('./contract.js').Shape\"",
    );
    let ambiguous = "interface Left { a: boolean; } interface Right { b: boolean; } export { Left as Shape, Right as Shape };";
    for (source, contract, selector, count) in [
        (INLINE, CONTRACT, ").Shape", 1),
        (NAMED, CONTRACT, "Input", 1),
        (&missing, CONTRACT, ").Shape", 0),
        (INLINE, &wrong_space, ").Shape", 0),
        (&nested, CONTRACT, ").Shape", 0),
        (&unplaced_owner, CONTRACT, ").Shape", 0),
        (&malformed, CONTRACT, ").Shape", 0),
        (&competing, CONTRACT, ").Shape", 0),
        (&type_query, CONTRACT, ").Shape", 0),
        (&quoted, CONTRACT, ").Shape", 0),
        (INLINE, ambiguous, ").Shape", 0),
    ] {
        let analysis = analyze(source, contract);
        let publication = analysis.publication();
        publication
            .validate_facts()
            .expect("valid type publication");
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == "lib/consumer.d.ts")
            .expect("written source unit");
        assert!(unit.source_complete && unit.source == source);
        let at = source.rfind(selector).expect("written source qualifier");
        let qualifier = selector.trim_start_matches(").");
        let start = u64::try_from(at + selector.len() - qualifier.len()).expect("byte offset");
        let end = start + u64::try_from(qualifier.len()).expect("byte length");
        let written = TextRange { start, end };
        let references = publication
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.unit == unit.unit && site.range == written)
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), count, "{source}");
        if count == 0 {
            continue;
        }
        let target = publication
            .declarations
            .iter()
            .find(|row| {
                row.symbol.as_str().ends_with("/Shape")
                    && row.unit.as_str().ends_with("/lib/contract.d.ts")
            })
            .expect("written target declaration");
        assert!(
            references
                .iter()
                .any(|edge| { edge.to == target.symbol && edge.from.as_str().ends_with("/read") })
        );
    }
}

#[test]
fn inline_import_type_qualifiers_wait_for_remaining_publication_capacity() {
    let (publication, _, semantics, _) = analyze(INLINE, CONTRACT).into_parts();
    let (references, prior): (Vec<_>, Vec<_>) = publication.edges.into_iter().partition(|edge| {
        edge.kinds == [EdgeKind::HasType] && edge.to.as_str().ends_with("contract.d.ts/Shape")
    });
    assert_eq!(references.len(), 1);
    assert!(!prior.is_empty());
    for spare in 0..=1 {
        let mut records = Records {
            objects: publication.objects.clone(),
            declarations: publication.declarations.clone(),
            units: publication.units.clone(),
            edges: prior.clone(),
            ..Records::default()
        };
        records.retain_checked_inline_import_type_edges(&semantics, prior.len() + spare);
        assert_eq!(&records.edges[..prior.len()], prior.as_slice());
        assert_eq!(records.edges.len(), prior.len() + spare);
        assert!(
            records.edges[prior.len()..]
                .iter()
                .all(|edge| references.contains(edge))
        );
    }
}
