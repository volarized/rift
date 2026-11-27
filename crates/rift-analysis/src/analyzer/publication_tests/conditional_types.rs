use super::*;

const SHAPES: &str = concat!(
    "export type Choice<T> = T extends { item: infer Value; fast: true }\n",
    "    ? Value\n",
    "    : T extends { item: infer Value; slow?: true }\n",
    "        ? Value\n",
    "        : never;\n",
    "export type State<T> = T extends string\n",
    "    ? { state: 'ready' }\n",
    "    : { state: 'waiting' };\n",
    "export type Stable = { state: number };\n",
    "export type Wrapped = Promise<{ key: string } | { key: number }>;\n",
);

fn analyze(path: &str, source: &str, entry: bool) -> PackageAnalysis {
    let extension = if path.ends_with(".d.cts") {
        "cjs"
    } else {
        "js"
    };
    let root = if entry { path } else { "index.d.ts" };
    let contents = format!("export type * from './lib/shape.{extension}';\n");
    let manifest =
        format!(r#"{{"name":"beacon","version":"1.0.0","type":"module","types":"./{root}"}}"#);
    fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.d.ts", &contents), (path, source)],
        None,
        Some(("package.json", &manifest)),
    )
    .expect("checked TypeScript package")
}

#[test]
fn typescript_secondary_aliases_keep_repeated_conditional_properties() {
    for suffix in ["d.ts", "d.cts", "ts"] {
        let path = format!("lib/shape.{suffix}");
        for entry in [true, false] {
            let analysis = analyze(&path, SHAPES, entry);
            let found = analysis.publication();
            found.validate_facts().expect("valid conditional types");
            let unit = found
                .units
                .iter()
                .find(|row| row.path.0 == path)
                .expect("source unit");
            assert!(unit.source_complete && unit.source == SHAPES);
            assert_branch_properties(
                found,
                &unit.unit,
                SHAPES,
                "Choice",
                &["item: infer Value"],
                entry,
            );
            assert_branch_properties(
                found,
                &unit.unit,
                SHAPES,
                "State",
                &["state: 'ready'", "state: 'waiting'"],
                entry,
            );
            assert_branch_properties(
                found,
                &unit.unit,
                SHAPES,
                "Wrapped",
                &["key: string", "key: number"],
                entry,
            );
            assert!(found.declarations.iter().any(|row| {
                row.symbol.as_str().ends_with("/Stable/state") && row.source == "state: number"
            }));
            for written in ["fast: true", "slow?: true"] {
                assert!(
                    found
                        .declarations
                        .iter()
                        .any(|row| row.source == written && row.unit == unit.unit)
                );
            }
            let uses = found
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds == [EdgeKind::HasType]
                        && edge.from.as_str().ends_with("/Choice")
                        && edge.to.as_str().contains("/Choice/$conditional%23")
                })
                .map(|edge| &edge.to)
                .collect::<BTreeSet<_>>();
            assert_eq!(uses.len(), 2);
        }
    }
}

fn assert_branch_properties(
    found: &PackagePublication,
    unit: &rift_protocol::read::SourceUnitId,
    source: &str,
    alias: &str,
    spellings: &[&str],
    shared: bool,
) {
    let owner = found
        .objects
        .iter()
        .find(|row| row.name == alias && row.kind.0 == "type_alias")
        .and_then(|row| row.id.as_ref())
        .expect("checked alias owner");
    let mut identities = BTreeSet::new();
    for spelling in spellings {
        for (offset, _) in source.match_indices(spelling) {
            let offset = u64::try_from(offset).expect("byte offset");
            let end = offset + u64::try_from(spelling.len()).expect("byte count");
            let member = found
                .declarations
                .iter()
                .find(|row| row.range == TextRange { start: offset, end } && &row.unit == unit)
                .expect("exact property declaration");
            let object = found
                .objects
                .iter()
                .find(|row| row.id.as_ref() == Some(&member.symbol))
                .expect("placed property object");
            assert_eq!(object.container.as_ref(), Some(owner));
            assert_eq!(object.kind.0, "property");
            identities.insert(&member.symbol);
        }
    }
    assert_eq!(identities.len(), if shared { 1 } else { 2 });
}

#[test]
fn typescript_conditional_properties_require_distinct_complete_bodies() {
    let cases = [
        "export type State<T> = T extends { value: T; value: T } ? { value: T } : { value: null };",
        "export type State<T> = { value: T } extends T ? { value: T } : { value: null };",
        "export type State<T> = T extends string ? Promise<{ value: T }> : Promise<{ value: null }>;",
        "export type State<T> = T extends { value: T } ? { value: T } : { value: ;",
    ];
    for source in cases {
        let analysis = analyze("lib/shape.d.ts", source, false);
        let found = analysis.publication();
        assert!(
            found
                .declarations
                .iter()
                .all(|row| !row.source.starts_with("value: ")),
            "{source}"
        );
    }
}
