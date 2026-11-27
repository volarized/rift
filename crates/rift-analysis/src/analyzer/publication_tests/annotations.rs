//! References within Python string annotations.

use super::*;
use rift_protocol::read::EdgeKind;

#[test]
fn package_python_compound_string_annotations_keep_written_type_ranges() {
    const CONSUMER: &str = concat!(
        "from typing import Literal\n",
        "from . import model\n",
        "def get(item: \"model.Item | None\") -> \"list[model.Item]\":\n",
        "    return [] if item is None else [item]\n",
        "def plain(item: \"model.Item\") -> model.Item:\n",
        "    return item\n",
        "def label() -> Literal[\"model.Item\"]:\n",
        "    return \"model.Item\"\n",
    );
    let files = [
        ("click/__init__.py", ""),
        ("click/model.py", "class Item: pass\n"),
        ("click/consumer.py", CONSUMER),
    ];
    let complete = publication(&files, None, &["click"]);
    let compound = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::HasType]
            && edge.from.as_str().ends_with("/consumer/get")
            && edge.to.as_str().ends_with("/model/Item")
    };
    let actual = complete
        .edges
        .iter()
        .filter(|edge| compound(edge))
        .map(|edge| {
            edge.occurrence
                .as_ref()
                .expect("authored type")
                .range
                .clone()
        })
        .collect::<Vec<_>>();
    let expected = ["model.Item | None", "model.Item]"].map(|site| {
        let start = CONSUMER.find(site).expect("written type");
        rift_protocol::read::TextRange {
            start: u64::try_from(start).expect("type start"),
            end: u64::try_from(start + "model.Item".len()).expect("type end"),
        }
    });
    assert_eq!(actual, expected);
    assert_eq!(
        complete
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str().ends_with("/consumer/plain")
                    && edge.to.as_str().ends_with("/model/Item")
            })
            .count(),
        2
    );
    assert!(!complete.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/consumer/label") && edge.to.as_str().ends_with("/model/Item")
    }));
    let alias_read = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/consumer/plain")
            && edge.to.as_str().ends_with("/model")
    };
    assert_direct_annotation_aliases(&complete, CONSUMER, alias_read);
    let prior = complete
        .edges
        .iter()
        .filter(|edge| !compound(edge) && !alias_read(edge))
        .cloned()
        .collect::<Vec<_>>();
    let capped = python_publication_with_limits(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(prior.len()),
    );
    assert_eq!(capped.declarations, complete.declarations);
    assert_eq!(capped.edges, prior);
}

fn assert_direct_annotation_aliases(
    complete: &PackagePublication,
    source: &str,
    alias_read: impl Fn(&rift_protocol::index::StoredEdge) -> bool,
) {
    let plain = source.find("def plain").expect("written function");
    let expected = source[plain..]
        .match_indices("model.Item")
        .take(2)
        .map(|(offset, _)| {
            let start = u64::try_from(plain + offset).expect("module offset");
            rift_protocol::read::TextRange {
                start,
                end: start + 5,
            }
        })
        .collect::<Vec<_>>();
    let actual = complete
        .edges
        .iter()
        .filter(|edge| alias_read(edge))
        .map(|edge| {
            edge.occurrence
                .as_ref()
                .expect("written module")
                .range
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert!(!complete.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/consumer/label")
            && edge.to.as_str().ends_with("/model")
    }));
}

#[test]
fn python_checked_legacy_type_aliases_keep_the_written_module() {
    const SOURCE: &str = concat!(
        "from typing import Literal\n",
        "from . import types as definitions\n",
        "from . import hidden\n",
        "from . import types as ambiguous\n",
        "from . import hidden as ambiguous\n",
        "def read(value: definitions.Callback) -> definitions.MaybeEntry:\n    return None\n",
        "def plain(value: definitions.Opaque) -> definitions.Entry:\n    return definitions.Entry()\n",
        "def text(item: Literal[\"definitions.Version\"]): pass\n",
        "def literal(item: Literal[definitions.Version]): pass\n",
        "def wrong_value(item: definitions.Version): pass\n",
        "def wrong_function(item: definitions.factory): pass\n",
        "def unresolved(item: ambiguous.Callback, shadow: hidden.Callback): pass\n",
        "def shadowed(definitions):\n    value: definitions.Callback\n    return value\n",
    );
    let files = [
        ("demo/__init__.py", ""),
        (
            "demo/types.py",
            concat!(
                "from typing import Any, Callable, Union\n",
                "class Entry: pass\n",
                "Callback = Callable[[Entry], bool]\n",
                "MaybeEntry = Union[Entry, None]\n",
                "Opaque = Any\n",
                "Version = 7\n",
                "def factory(): return Entry()\n",
            ),
        ),
        (
            "demo/hidden.py",
            "Callable = custom\nCallback = Callable[object, int]\n",
        ),
        ("demo/consumer.py", SOURCE),
    ];
    let publication = publication(&files, None, &["demo"]);
    let module = |edge: &rift_protocol::index::StoredEdge| {
        edge.to.as_str().ends_with("/demo/types")
            && edge.kinds == [EdgeKind::Reads]
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit.0.ends_with("/demo/consumer.py"))
    };
    let expected = [
        "definitions.Callback",
        "definitions.MaybeEntry",
        "definitions.Opaque",
        "definitions.Entry",
        "definitions.Entry()",
    ]
    .map(|site| {
        u64::try_from(SOURCE.find(site).expect("written qualifier")).expect("source offset")
    });
    let mut actual = publication
        .edges
        .iter()
        .filter(|edge| module(edge))
        .map(|edge| {
            let range = &edge.occurrence.as_ref().expect("written module").range;
            assert_eq!(range.end - range.start, 11);
            range.start
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, expected);
    for (alias, offset) in ["Callback", "MaybeEntry", "Opaque"]
        .into_iter()
        .zip(expected)
    {
        assert!(publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::HasType]
                && edge.to.as_str().ends_with(&format!("/demo/types/{alias}"))
                && edge.occurrence.as_ref().is_some_and(|site| {
                    site.range.start == offset && site.unit.0.ends_with("/demo/consumer.py")
                })
        }));
    }
    assert!(publication.edges.iter().all(|edge| {
        edge.occurrence.as_ref().is_none_or(|site| {
            !site.unit.0.ends_with("/demo/consumer.py")
                || !edge.to.as_str().ends_with("/demo/hidden")
                || edge.kinds != [EdgeKind::Reads]
        })
    }));
}
