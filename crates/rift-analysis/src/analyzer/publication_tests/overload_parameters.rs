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

#[test]
fn package_python_checked_overload_parameters_keep_their_physical_sites() {
    const FAMILY: &str = concat!(
        "        @typing.overload\n",
        "        def choose(self, value: int) -> int: ...\n",
        "        @typing.overload\n",
        "        def choose(self, value: str) -> str: ...\n",
        "        def choose(self, value):\n",
        "            return value\n",
    );
    for (prefix, condition, tail, expected) in [
        ("import typing\n", "typing.TYPE_CHECKING", "", 3),
        (
            "import typing\nfrom typing import TYPE_CHECKING as checked\n",
            "checked",
            "",
            3,
        ),
        ("import typing\n", "enabled", "", 0),
        (
            "import typing\ntyping = object()\n",
            "typing.TYPE_CHECKING",
            "",
            0,
        ),
        (
            "import typing\n",
            "typing.TYPE_CHECKING",
            "    if typing.TYPE_CHECKING:\n        def choose(self, value): pass\n",
            0,
        ),
    ] {
        let source = format!("{prefix}class Item:\n    if {condition}:\n{FAMILY}{tail}");
        let result = publication(&[("click.py", source.as_str())], None, &["click"]);
        let declarations = result
            .declarations
            .iter()
            .filter(|entry| parameter(entry.symbol.as_str()))
            .map(|entry| entry.range.clone())
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), expected, "{source}");
        if expected == 0 {
            continue;
        }
        assert_eq!(
            declarations,
            source
                .match_indices("def choose(self, ")
                .map(|(start, prefix)| name_range(start + prefix.len()))
                .collect::<Vec<_>>(),
            "{source}"
        );
        let read = name_range(source.find("return value").expect("implementation") + 7);
        assert!(result.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.from.as_str().ends_with("/Item/choose")
                && parameter(edge.to.as_str())
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range == read)
        }));
    }
}

#[test]
fn package_python_stub_overloads_keep_each_written_parameter() {
    const STUB: &str = concat!(
        "from typing import overload\n",
        "@overload\n",
        "def parse(value: int) -> int: ...\n",
        "@overload\n",
        "def parse(value: str) -> str: pass\n",
        "class Item:\n",
        "    @overload\n",
        "    def choose(self, value: int) -> int: ...\n",
        "    @overload\n",
        "    def choose(self, value: str) -> str: ...\n",
        "    def control(self, checked: int) -> int: ...\n",
        "@overload\n",
        "def executable(argument: int) -> int: ...\n",
        "@overload\n",
        "def executable(argument: str) -> str: return argument\n",
        "from another_module import overload as unrelated\n",
        "@unrelated\n",
        "def ambiguous(binding: int) -> int: ...\n",
        "@unrelated\n",
        "def ambiguous(binding: str) -> str: ...\n",
    );

    let published = publication(&[("click/__init__.pyi", STUB)], None, &["click"]);
    let control = published
        .declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/Item/control/checked"))
        .expect("ordinary parameter in the same stub");
    assert_eq!(control.source, "checked");

    let mut expected = Vec::new();
    for (prefix, owner) in [("def parse(", "parse"), ("def choose(", "Item/choose")] {
        for (start, matched) in STUB.match_indices(prefix) {
            let start = start + matched.len();
            if owner == "Item/choose" {
                expected.push((format!("{owner}/self"), start, start + 4));
            }
            let value = start + STUB[start..].find("value:").expect("value parameter");
            expected.push((format!("{owner}/value"), value, value + 5));
        }
    }
    for (owner, start, end) in &expected {
        let range = TextRange {
            start: u64::try_from(*start).expect("source start"),
            end: u64::try_from(*end).expect("source end"),
        };
        let row = published
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with(owner) && row.range == range)
            .unwrap_or_else(|| panic!("written {owner} parameter at {range:?}"));
        assert_eq!(row.unit, control.unit);
        assert_eq!(row.source, STUB[*start..*end]);
    }
    assert_eq!(
        published
            .declarations
            .iter()
            .filter(|row| {
                expected
                    .iter()
                    .any(|(owner, _, _)| row.symbol.as_str().ends_with(owner))
            })
            .count(),
        expected.len()
    );

    assert!(!published.declarations.iter().any(|row| {
        row.symbol.as_str().ends_with("/executable/argument")
            || row.symbol.as_str().ends_with("/ambiguous/binding")
    }));
    let runtime = publication(&[("click/__init__.py", STUB)], None, &["click"]);
    assert!(!runtime.declarations.iter().any(|row| {
        expected
            .iter()
            .any(|(owner, _, _)| row.symbol.as_str().ends_with(owner))
    }));
}
