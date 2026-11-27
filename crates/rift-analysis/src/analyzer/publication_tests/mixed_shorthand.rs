//! Source bindings and shorthand properties in a placed `CommonJS` file.

use super::*;
use rift_protocol::read::TextRange;

const COMMONJS: &str = concat!(
    "function consume(value) { return value; }\n",
    "function apply(options) {\n",
    "    const { flag } = options;\n",
    "    return consume({ flag, keyed: true });\n",
    "}\n",
    "exports.apply = apply;\n",
);
const ESM: &str = "export function elsewhere() { return 0; }\n";
const SOLE: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"cjs/index.js"}"#;
const DUAL: &str = r#"{"name":"beacon","version":"1.0.0","main":"cjs/index.js","module":"esm/index.js","exports":{".":{"require":"./cjs/index.js","import":"./esm/index.js"}}}"#;

#[test]
fn independently_placed_commonjs_keeps_binding_and_shorthand_property() {
    let only = publish(vec![("cjs/index.js", COMMONJS)], SOLE);
    let dual = publish(
        vec![("esm/index.js", ESM), ("cjs/index.js", COMMONJS)],
        DUAL,
    );
    let reversed = publish(
        vec![("cjs/index.js", COMMONJS), ("esm/index.js", ESM)],
        DUAL,
    );
    assert_eq!(dual, reversed);
    let flag = |literal| {
        let start = u64::try_from(COMMONJS.find(literal).expect("written identifier") + 2)
            .expect("written offset");
        TextRange {
            start,
            end: start + 4,
        }
    };
    let pattern = flag("{ flag }");
    let shorthand = flag("{ flag,");
    for (publication, distinct) in [(&only, false), (&dual, true)] {
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == "cjs/index.js")
            .expect("selected CommonJS file");
        assert!(unit.source_complete);
        assert_eq!(unit.source, COMMONJS);
        for suffix in ["/apply", "/apply/keyed"] {
            assert!(
                publication
                    .declarations
                    .iter()
                    .any(|row| row.unit == unit.unit && row.symbol.as_str().ends_with(suffix))
            );
        }
        let declare = |range, expected| {
            publication
                .declarations
                .iter()
                .find(|row| {
                    row.unit == unit.unit
                        && &row.range == range
                        && row.syntax_element_kind.as_deref() == Some(expected)
                })
                .expect("exact written declaration")
        };
        let binding = declare(&pattern, "shorthand_property_identifier_pattern");
        let member = declare(&shorthand, "shorthand_property_identifier");
        if distinct {
            assert_ne!(binding.symbol, member.symbol);
        }
    }
}

#[test]
fn independently_placed_commonjs_refuses_competing_binding() {
    let source = COMMONJS
        .replace(
            "    return consume",
            "    { const flag = false; return consume",
        )
        .replace("keyed: true });", "keyed: true }); }");
    let publication = publish(vec![("cjs/index.js", &source), ("esm/index.js", ESM)], DUAL);
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "cjs/index.js")
        .expect("checked file")
        .unit;
    let target = source.find("{ flag,").expect("written shorthand") + 2;
    let start = u64::try_from(target).expect("checked offset");
    assert!(publication.declarations.iter().all(|row| {
        &row.unit != unit
            || row.range.start != start
            || row.syntax_element_kind.as_deref() != Some("shorthand_property_identifier")
    }));
    assert!(
        publication
            .declarations
            .iter()
            .any(|row| &row.unit == unit && row.symbol.as_str().ends_with("/apply/keyed"))
    );
}

#[test]
fn independently_placed_commonjs_keeps_distinct_literal_members() {
    let source = COMMONJS.replace(
        "    return consume",
        "    if (options.alternate) return { flag };\n    return consume",
    );
    let publication = publish(vec![("cjs/index.js", &source), ("esm/index.js", ESM)], DUAL);
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "cjs/index.js")
        .expect("selected file")
        .unit;
    let bindings = publication
        .declarations
        .iter()
        .filter(|row| &row.unit == unit && row.source == "flag")
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 3);
    assert_eq!(
        bindings
            .iter()
            .map(|row| &row.symbol)
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );

    let duplicate = COMMONJS.replace("{ flag, keyed", "{ flag, flag, keyed");
    let publication = publish(
        vec![("cjs/index.js", &duplicate), ("esm/index.js", ESM)],
        DUAL,
    );
    assert!(
        publication
            .declarations
            .iter()
            .all(|row| row.source != "flag")
    );
}

fn publish(files: Vec<(&str, &str)>, manifest: &str) -> PackagePublication {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        files,
        None,
        Some(("package.json", manifest)),
    )
    .expect("checked npm package");
    let publication = analysis.into_parts().0;
    publication
        .validate_facts()
        .expect("valid declaration facts");
    publication
}

#[test]
fn independently_placed_commonjs_keeps_the_prior_source_budget() {
    let room = bounded(u64::MAX);
    let extra = room
        .declarations
        .iter()
        .filter(|row| row.source == "flag")
        .map(|row| &row.symbol)
        .collect::<BTreeSet<_>>();
    assert_eq!(extra.len(), 2);
    let primary = room
        .declarations
        .iter()
        .filter(|row| !extra.contains(&row.symbol))
        .cloned()
        .collect::<Vec<_>>();
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
        .map(|source| u64::try_from(source.len()).expect("retained source bytes"))
        .sum();
    let exact = bounded(bytes);
    assert_eq!(exact.declarations, primary);
    assert_eq!(exact.units, room.units);
    assert_eq!(exact.documents, room.documents);
    assert_eq!(
        exact.edges,
        room.edges
            .into_iter()
            .filter(|edge| !extra.contains(&edge.from) && !extra.contains(&edge.to))
            .collect::<Vec<_>>()
    );
}

fn bounded(retained_bytes: u64) -> PackagePublication {
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("package owner");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let cjs = ProjectPath::new("cjs/index.js").expect("CommonJS source");
    let esm = ProjectPath::new("esm/index.js").expect("ECMAScript source");
    let manifest = ProjectPath::new("package.json").expect("package manifest");
    let files = [
        PackageSource::new(&cjs, COMMONJS),
        PackageSource::new(&esm, ESM),
    ];
    let metadata = [PackageSource::new(&manifest, DUAL)];
    let limits = ExactPackageLimits::new(3, 4096)
        .with_retained_source_bytes(192, retained_bytes)
        .expect("retained source bounds");
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
        .expect("package source")
        .with_framework_context(&metadata, &[])
        .expect("npm manifest");
    PackageAnalyzer::analyze(input, 1)
        .expect("bounded package publication")
        .into_parts()
        .0
}
