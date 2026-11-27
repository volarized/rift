//! Written default and rest parameters of JavaScript arrow initializers.

use super::*;

const BODY: &str = concat!(
    "const seed = 1;\n",
    "const read = (plain, fallback = seed, ...rest) => [plain, fallback, rest];\n",
    "function ordinary(value = seed, ...items) { return [value, items]; }\n",
    "function register(fn) { return fn; }\n",
    "const registered = register((anonymous = seed, ...unplaced) => [anonymous, unplaced]);\n",
    "let delayed;\n",
    "delayed = (assigned = seed) => assigned;\n",
    "var competing = (first = seed) => first;\n",
    "var competing = (...second) => second;\n",
);

fn range(start: usize, name: &str) -> TextRange {
    let start = u64::try_from(start).expect("authored byte offset");
    TextRange {
        start,
        end: start + u64::try_from(name.len()).expect("authored name length"),
    }
}

#[test]
fn named_arrow_initializer_keeps_wrapped_parameters_in_its_written_scope() {
    for (entry, source_path, driver, exports) in [
        ("index.mjs", "index.mjs", "", "export { read, ordinary };"),
        (
            "index.mjs",
            "lib/worker.mjs",
            "export { read, ordinary } from './lib/worker.mjs';",
            "export { read, ordinary };",
        ),
        (
            "index.cjs",
            "lib/worker.cjs",
            "const worker = require('./lib/worker.cjs'); module.exports = worker;",
            "module.exports = { read, ordinary };",
        ),
    ] {
        let source = format!("{BODY}{exports}\n");
        let mut files = vec![(source_path, source.as_str())];
        if source_path != entry {
            files.push((entry, driver));
        }
        let manifest =
            format!(r#"{{"name":"beacon","version":"1.0.0","type":"module","main":"./{entry}"}}"#);
        let result = fixture::package_result_with_metadata(
            ShippedLanguage::JavaScript,
            files,
            None,
            Some(("package.json", &manifest)),
        )
        .expect("selected module");
        let publication = result.publication();
        publication
            .validate_facts()
            .expect("written arrow bindings");
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == source_path)
            .expect("complete arrow source");
        assert!(unit.source_complete && unit.source == source);
        let rows = publication
            .declarations
            .iter()
            .filter(|row| row.unit == unit.unit);
        for (owner, name, header, body) in [
            ("read", "plain", "plain,", "[plain"),
            ("read", "fallback", "fallback =", ", fallback,"),
            ("read", "rest", "rest) =>", ", rest]"),
            ("ordinary", "value", "value =", "[value"),
            ("ordinary", "items", "items) {", ", items]"),
        ] {
            let suffix = format!("/{owner}/{name}");
            let declaration = rows
                .clone()
                .find(|row| row.symbol.as_str().ends_with(&suffix))
                .expect("written parameter");
            let start = source.find(header).expect("written parameter");
            assert_eq!(declaration.range, range(start, name));
            assert_eq!(declaration.source, name);
            let owner = publication
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&declaration.symbol))
                .filter(|object| object.kinds.contains(&SymbolKind::Parameter))
                .and_then(|object| object.container.as_ref())
                .expect("checked parameter owner");
            let start = source.find(body).expect("body use") + body.find(name).expect("name");
            assert!(publication.edges.iter().any(|edge| {
                edge.from == *owner
                    && edge.to == declaration.symbol
                    && edge.kinds == [EdgeKind::Reads]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.unit == unit.unit && site.range == range(start, name)
                    })
            }));
        }
        let seed = rows
            .clone()
            .find(|row| row.symbol.as_str().ends_with("/seed"))
            .expect("written default expression target");
        let start =
            source.find("fallback = seed").expect("default expression") + "fallback = ".len();
        assert!(publication.edges.iter().any(|edge| {
            edge.to == seed.symbol
                && edge.kinds == [EdgeKind::Reads]
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == unit.unit && site.range == range(start, "seed")
                })
        }));
        for (header, name) in [
            ("anonymous =", "anonymous"),
            ("unplaced)", "unplaced"),
            ("assigned =", "assigned"),
            ("first =", "first"),
            ("second) =>", "second"),
        ] {
            let start = source.find(header).expect("unplaced parameter");
            assert!(rows.clone().all(|row| row.range != range(start, name)));
        }
    }
}

#[test]
fn wrapped_arrow_parameters_follow_established_declaration_slots() {
    let mut identity = fixture::identity();
    identity.manager = "npm".to_owned();
    identity.registry = "registry.npmjs.org".to_owned();
    let owner = identity.owner().expect("package owner");
    let origin = fixture::origin(&identity);
    let language = ShippedLanguage::JavaScript.language();
    let path = ProjectPath::new("index.mjs").expect("entry path");
    let manifest = ProjectPath::new("package.json").expect("manifest path");
    let files = [PackageSource::new(
        &path,
        "export function keep(old = 0) { return old; }\nexport const read = (plain, newValue = 0, ...items) => [plain, newValue, items];\n",
    )];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#,
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
    let established = full
        .declarations
        .iter()
        .filter(|row| !matches!(row.source.as_str(), "newValue" | "items"))
        .collect::<Vec<_>>();
    assert_eq!(full.declarations.len(), established.len() + 2);
    assert!(established.iter().any(|row| row.source == "old"));
    let bound = collect(Some(
        u32::try_from(established.len()).expect("remaining slots"),
    ));
    assert_eq!(bound.declarations.iter().collect::<Vec<_>>(), established);
}
