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
    let union_parameter = INLINE.replace(").Shape): number", ").Shape | null): number");
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
        (&union_parameter, CONTRACT, ").Shape", 1),
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

#[test]
fn inline_import_specifiers_keep_the_written_declaration_module() {
    let double = INLINE.replace("'./contract.js'", "\"./contract.js\"");
    let missing = INLINE.replace("contract.js", "absent.js");
    let nested = INLINE.replace(").Shape", ").Shape.Width");
    let unqualified = INLINE.replace(").Shape", ")");
    let query = INLINE.replace("import(", "typeof import(");
    for (source, literal, count) in [
        (INLINE, "'./contract.js'", 1),
        (double.as_str(), "\"./contract.js\"", 1),
        (NAMED, "'./contract.js'", 1),
        (missing.as_str(), "'./absent.js'", 0),
        (nested.as_str(), "'./contract.js'", 0),
        (unqualified.as_str(), "'./contract.js'", 0),
        (query.as_str(), "'./contract.js'", 0),
    ] {
        let analysis = analyze(source, CONTRACT);
        let publication = analysis.publication();
        publication
            .validate_facts()
            .expect("valid type publication");
        let unit = publication
            .units
            .iter()
            .find(|row| row.path.0 == "lib/consumer.d.ts")
            .expect("written consumer");
        let start = u64::try_from(source.find(literal).expect("written literal")).expect("start");
        let end = start + u64::try_from(literal.len()).expect("length");
        let imports = publication
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::Imports]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == unit.unit && site.range == TextRange { start, end }
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(imports.len(), count, "{source}");
        if let [edge] = imports.as_slice() {
            let source = publication
                .declarations
                .iter()
                .find(|row| row.symbol == edge.from && row.unit == unit.unit)
                .expect("retained source module");
            let target = publication
                .declarations
                .iter()
                .find(|row| {
                    row.symbol == edge.to && row.unit.as_str().ends_with("/lib/contract.d.ts")
                })
                .expect("retained declaration module");
            assert!(source.source_complete && source.range.start == 0);
            assert!(target.source_complete && target.range.start == 0);
        }
    }
}

#[test]
fn inline_import_module_edges_keep_earlier_edges_and_require_complete_modules() {
    let (publication, _, semantics, _) = analyze(INLINE, CONTRACT).into_parts();
    let (modules, prior): (Vec<_>, Vec<_>) = publication.edges.into_iter().partition(|edge| {
        edge.kinds == [EdgeKind::Imports] && edge.to.as_str().ends_with("%2Flib%2Fcontract.d.ts")
    });
    let [module] = modules.as_slice() else {
        panic!("one checked inline import module");
    };
    assert!(!prior.is_empty());
    for (spare, incomplete) in [
        (0, None),
        (1, None),
        (1, Some(&module.from)),
        (1, Some(&module.to)),
    ] {
        let mut records = Records {
            objects: publication.objects.clone(),
            declarations: publication.declarations.clone(),
            units: publication.units.clone(),
            edges: prior.clone(),
            ..Records::default()
        };
        if let Some(id) = incomplete {
            let row = records
                .declarations
                .iter_mut()
                .find(|row| &row.symbol == id)
                .expect("complete module before the control");
            row.source_complete = false;
        }
        records.retain_checked_inline_import_modules(&semantics, prior.len() + spare);
        assert_eq!(&records.edges[..prior.len()], prior.as_slice());
        let new = if spare == 0 || incomplete.is_some() {
            &[][..]
        } else {
            modules.as_slice()
        };
        assert_eq!(&records.edges[prior.len()..], new);
    }
}

#[test]
fn checked_inline_import_type_alias_keeps_the_full_written_declaration() {
    const PREFIX: &str = "// declaration\n";
    const NEIGHBOR: &str = concat!(
        "export type Independent = string;\n",
        "export declare function read(value: Public): Public;\n",
    );
    const INLINE_ALIAS: &str = "type Public = import('./contract.js').Shape;";
    const UNKNOWN_ALIAS: &str = "type Public = import('./missing.js').Shape;";
    const INTERSECTION_ALIAS: &str =
        "type Public = import('./contract.js').Shape & { readonly width: number };";
    const UNION_ALIAS: &str = "type Public = import('./contract.js').Shape | null;";
    const BARE_ALIAS: &str = "type Public = import('transport').Shape;";
    const BARE_INTERSECTION_ALIAS: &str =
        "type Public = import('transport').Shape & { readonly width: number };";
    const NAMED_ALIAS: &str = "type Public = Input;";
    for (written, resolved, canonical) in [
        (
            format!("{PREFIX}export {INLINE_ALIAS}\n{NEIGHBOR}"),
            true,
            Some(INLINE_ALIAS),
        ),
        (
            format!("{PREFIX}export {UNKNOWN_ALIAS}\n{NEIGHBOR}"),
            false,
            Some(UNKNOWN_ALIAS),
        ),
        (
            format!("{PREFIX}export {INTERSECTION_ALIAS}\n{NEIGHBOR}"),
            true,
            Some(INTERSECTION_ALIAS),
        ),
        (
            format!("{PREFIX}export {UNION_ALIAS}\n{NEIGHBOR}"),
            true,
            Some(UNION_ALIAS),
        ),
        (
            format!("{PREFIX}export {BARE_ALIAS}\n{NEIGHBOR}"),
            false,
            Some(BARE_ALIAS),
        ),
        (
            format!("{PREFIX}export {BARE_INTERSECTION_ALIAS}\n{NEIGHBOR}"),
            false,
            Some(BARE_INTERSECTION_ALIAS),
        ),
        (
            format!(
                "{PREFIX}import type {{ Shape as Input }} from './contract.js';\nexport {NAMED_ALIAS}\n{NEIGHBOR}"
            ),
            true,
            Some(NAMED_ALIAS),
        ),
        (
            format!("{PREFIX}export type Public = import('./contract.js').;\n{NEIGHBOR}"),
            false,
            None,
        ),
        (
            format!("{PREFIX}export type Public = import('transport').Shape & ;\n{NEIGHBOR}"),
            false,
            None,
        ),
    ] {
        assert_checked_inline_alias(&written, resolved, canonical, canonical.is_none());
    }
}

fn assert_checked_inline_alias(
    written: &str,
    resolved: bool,
    canonical: Option<&str>,
    syntax_has_errors: bool,
) {
    let (publication, files, _, _) = analyze(written, CONTRACT).into_parts();
    publication
        .validate_facts()
        .expect("valid declaration publication");
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "lib/consumer.d.ts")
        .expect("physically retained declaration source");
    assert!(unit.source_complete && unit.source == written);
    let source = files
        .iter()
        .find(|file| file.file().path().as_str() == "lib/consumer.d.ts")
        .expect("syntax source");
    assert_eq!(source.file().syntax().has_errors(), syntax_has_errors);
    let physical = |name: &str| {
        publication.declarations.iter().find(|row| {
            row.unit == unit.unit
                && row
                    .symbol
                    .as_str()
                    .ends_with(&format!("consumer.d.ts/{name}"))
        })
    };
    let stable = physical("Independent").expect("neighbor declaration");
    assert_eq!(stable.source, "type Independent = string;");
    let Some(expected) = canonical else {
        assert!(physical("Public").is_none());
        return;
    };
    let declared = physical("Public").expect("checked alias");
    let start = u64::try_from(written.find(expected).expect("authored item")).expect("offset");
    let end = start + u64::try_from(expected.len()).expect("length");
    assert_eq!(declared.range, TextRange { start, end });
    assert_eq!(declared.source, expected);
    assert_eq!(
        declared.syntax_element_kind.as_deref(),
        Some("type_alias_declaration")
    );
    assert_eq!(
        publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::HasType]
                && edge.from == declared.symbol
                && edge.to.as_str().ends_with("contract.d.ts/Shape")
        }),
        resolved
    );
    let use_text = "export declare function read(value: Public): Public;";
    let use_start = written.find(use_text).expect("written signature");
    let written_types = use_text
        .match_indices("Public")
        .map(|(offset, token)| {
            let start = u64::try_from(use_start + offset).expect("written use offset");
            TextRange {
                start,
                end: start + u64::try_from(token.len()).expect("written use length"),
            }
        })
        .collect::<Vec<_>>();
    let mut retained = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::HasType]
                && edge.from.as_str().ends_with("/read")
                && edge.to == declared.symbol
        })
        .map(|edge| {
            let occurrence = edge.occurrence.as_ref().expect("written type use");
            assert_eq!(occurrence.unit, unit.unit);
            occurrence.range.clone()
        })
        .collect::<Vec<_>>();
    retained.sort_by_key(|range| range.start);
    assert_eq!(retained, written_types);
}

#[test]
fn checked_inline_import_type_alias_does_not_recover_a_competing_owner() {
    const SOURCE: &str = concat!(
        "export type Public = import('./contract.js').Shape;\n",
        "export type Public = number;\n",
    );
    let publication = analyze(SOURCE, CONTRACT);
    let original = publication
        .publication()
        .declarations
        .iter()
        .filter(|row| {
            row.unit.as_str().ends_with("/lib/consumer.d.ts")
                && row.symbol.as_str().ends_with("consumer.d.ts/Public")
        })
        .map(|row| row.source.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        original,
        [
            "type Public = import('./contract.js').Shape;",
            "type Public = number;",
        ],
    );
    assert!(!publication.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::HasType] && edge.from.as_str().ends_with("consumer.d.ts/Public")
    }));
}
