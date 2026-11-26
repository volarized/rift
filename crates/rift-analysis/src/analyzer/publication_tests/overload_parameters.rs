//! Physical declarations and reads in a Python overload family.

use super::*;
use rift_protocol::configuration::PackageConfiguration;
use rift_protocol::read::{EdgeKind, TextRange};

const SOURCE: &str = concat!(
    "from typing import overload as signature\n",
    "def ordinary(argument): return argument\n",
    "@signature\n",
    "def choose(value: int) -> int: ...\n",
    "@signature\n",
    "def choose(value: str) -> str: ...\n",
    "def choose(value):\n",
    "    return value\n",
    "def run(): return choose(1)\n",
    "def repeat(value): return value\n",
    "def repeat(value): return value\n",
);

fn parameter(id: &str) -> bool {
    id.ends_with("/choose/value")
}

fn name_range(start: usize) -> TextRange {
    let start = u64::try_from(start).expect("source position");
    TextRange {
        start,
        end: start + 5,
    }
}

#[test]
fn package_python_overloads_keep_each_written_parameter_and_the_implementation_read() {
    let files = [("click.py", SOURCE)];
    let published = publication(&files, None, &["click"]);
    let ranges = published
        .declarations
        .iter()
        .filter(|declaration| parameter(declaration.symbol.as_str()))
        .map(|declaration| {
            assert_eq!(declaration.source, "value");
            declaration.range.clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ranges,
        SOURCE
            .match_indices("def choose(")
            .map(|(start, prefix)| name_range(start + prefix.len()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        published
            .declarations
            .iter()
            .filter(|declaration| declaration.symbol.as_str().ends_with("/choose"))
            .count(),
        3
    );
    assert!(
        !published
            .declarations
            .iter()
            .any(|declaration| declaration.symbol.as_str().ends_with("/repeat/value"))
    );
    let read = name_range(SOURCE.find("return value").expect("implementation body") + 7);
    assert!(published.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/choose")
            && parameter(edge.to.as_str())
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.range == read)
    }));
    assert!(published.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/run")
            && edge.to.as_str().ends_with("/choose")
    }));
    let prior_declarations = published
        .declarations
        .iter()
        .filter(|entry| !parameter(entry.symbol.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let prior_edges = published
        .edges
        .iter()
        .filter(|edge| !parameter(edge.to.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        &files,
        None,
        &["click"],
        PackageConfiguration::default(),
        Some(prior_edges.len()),
    );
    assert_eq!(limited.declarations, published.declarations);
    assert_eq!(limited.edges, prior_edges);

    let limited = publication_with_configuration(
        &files,
        None,
        &["click"],
        PackageConfiguration {
            symbols: u32::try_from(prior_declarations.len()).expect("fixture count"),
            ..PackageConfiguration::default()
        },
    );
    assert_eq!(limited.declarations, prior_declarations);
    assert_eq!(limited.edges, prior_edges);
}

#[test]
fn package_python_overload_parameters_refuse_competing_declarations() {
    const SOURCE: &str = concat!(
        "from typing import overload, overload as shadowed\n",
        "def shadowed(function): return function\n",
        "@shadowed\n",
        "def ordinary(value: int) -> int: ...\n",
        "def ordinary(value): return value\n",
        "@overload\n",
        "def choose(value: int) -> int: ...\n",
        "@overload\n",
        "def choose(value: str) -> str: ...\n",
        "def choose(value):\n",
        "    value = 1\n",
        "    return value\n",
    );
    let published = publication(&[("click.py", SOURCE)], None, &["click"]);
    assert!(
        !published
            .declarations
            .iter()
            .any(|entry| entry.symbol.as_str().ends_with("/ordinary/value"))
    );
    assert_eq!(
        published
            .declarations
            .iter()
            .filter(|entry| parameter(entry.symbol.as_str()))
            .map(|entry| entry.source.as_str())
            .collect::<Vec<_>>(),
        ["value = 1"]
    );
}
