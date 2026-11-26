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
    let prior = complete
        .edges
        .iter()
        .filter(|edge| !compound(edge))
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
