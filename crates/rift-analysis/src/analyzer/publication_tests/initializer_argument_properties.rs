//! Written properties in immediately invoked JavaScript initializers.

use super::*;
use rift_protocol::read::EdgeKind;

const MANIFEST: &str =
    r#"{"name":"beacon","version":"1.0.0","type":"module","main":"./index.mjs"}"#;
const ENTRY: &str = "export { outer } from './worker.mjs';";
const CALL: &str = "keep([{ tag: 1 }, /* second slot */ { tag: 2 }])";
const KEEP: &str = "function keep(items) { return items; }\n";

fn collect(worker: &str, edge_limit: usize) -> PackageAnalysis {
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("package owner");
    let origin = fixture::origin(&package);
    let language = ShippedLanguage::JavaScript.language();
    let entry = ProjectPath::new("index.mjs").expect("entry path");
    let source = ProjectPath::new("worker.mjs").expect("secondary module path");
    let manifest = ProjectPath::new("package.json").expect("manifest path");
    let sources = [
        PackageSource::new(&entry, ENTRY),
        PackageSource::new(&source, worker),
    ];
    let context = [PackageSource::new(&manifest, MANIFEST)];
    let limits = ExactPackageLimits::new(3, 4096)
        .with_edges(edge_limit)
        .expect("edge limit");
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("checked sources")
        .with_framework_context(&context, &[])
        .expect("package context");
    PackageAnalyzer::analyze(input, 1).expect("package graph")
}

#[test]
fn assigned_immediate_javascript_initializers_keep_separate_call_array_properties() {
    for function in ["function ()", "() =>"] {
        let source = format!("{KEEP}export const outer = ({function} {{ {CALL}; return 1; }})();");
        let analysis = collect(&source, 4096);
        let graph = analysis.publication();
        graph.validate_facts().expect("checked initializer facts");
        let unit = graph
            .units
            .iter()
            .find(|unit| unit.path.0 == "worker.mjs")
            .expect("retained secondary unit");
        let located = |prefix: &str, syntax: &str| {
            graph
                .declarations
                .iter()
                .find(|row| {
                    row.unit == unit.unit
                        && row
                            .syntax_element_kind
                            .as_ref()
                            .is_some_and(|kind| kind.as_str() == syntax)
                        && row.source.starts_with(prefix)
                })
                .expect("declared secondary owner")
        };
        let owner = located("outer = (", "variable_declarator");
        let callee = located("function keep(", "function_declaration");
        let mut distinct = Vec::new();
        for property in ["tag: 1", "tag: 2"] {
            let start =
                u64::try_from(source.find(property).expect("written key")).expect("fixture range");
            let row = graph
                .declarations
                .iter()
                .find(|row| {
                    row.unit == owner.unit && row.range.start == start && row.source == property
                })
                .expect("exact physical property");
            assert!(graph.objects.iter().any(|object| {
                object.id.as_ref() == Some(&row.symbol)
                    && object.container.as_ref() == Some(&owner.symbol)
            }));
            distinct.push(&row.symbol);
        }
        assert_ne!(distinct[0], distinct[1]);
        assert!(graph.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from == owner.symbol
                && edge.to == callee.symbol
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.unit == owner.unit
                        && usize::try_from(site.range.start).ok() == source.find(CALL)
                })
        }));
        let limited = collect(&source, 1);
        assert_eq!(limited.publication().declarations, graph.declarations);
        assert!(
            limited
                .publication()
                .edges
                .iter()
                .all(|edge| graph.edges.contains(edge))
        );
    }
}

#[test]
fn immediate_initializer_properties_require_a_placed_stable_owner_and_direct_argument() {
    let forms = [
        format!("export const outer = keep(function () {{ {CALL}; return 1; }});"),
        format!("export let outer = (() => {{ {CALL}; return 1; }})(); outer = 2;"),
        String::from(
            "export const outer = (() => { keep['call']([{ tag: 1 }, { tag: 2 }]); return 1; })();",
        ),
    ];
    for (index, form) in forms.into_iter().enumerate() {
        let source = format!("{KEEP}{form}");
        let result = collect(&source, 4096);
        let rows = result
            .publication()
            .declarations
            .iter()
            .filter(|row| matches!(row.source.as_str(), "tag: 1" | "tag: 2"))
            .collect::<Vec<_>>();
        if index == 0 {
            assert_eq!(rows.len(), 2, "existing checked callback declarations");
            assert!(
                rows.iter()
                    .all(|row| row.symbol.as_str().contains("$callback"))
            );
        } else {
            assert!(rows.is_empty(), "{form}");
        }
    }
}
