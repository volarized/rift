//! Authored Python initializer keyword references.

use super::*;
use rift_protocol::read::EdgeKind;

#[test]
fn package_python_dataclass_keywords_reference_generated_fields() {
    const SOURCE: &str = concat!(
        "import dataclasses as dc\n",
        "from dataclasses import dataclass as generated\n",
        "from attrs import define\n",
        "@generated\nclass Base:\n    label: str\n",
        "@dc.dataclass(kw_only=True)\nclass Options(Base):\n    value: int\n",
        "@generated(init=False)\nclass Disabled:\n    value: int\n",
        "@generated\nclass Custom:\n    value: int\n",
        "    def __init__(self, value): pass\n",
        "@generated\nclass Replaced:\n    value: int\n",
        "Replaced.__init__ = Custom.__init__\n",
        "@define\nclass Existing:\n    value: int\n",
        "def make(): return Options(label='name', value=1)\n",
        "def existing(): return Existing(value=2)\n",
        "def refused(): return Disabled(value=1), Custom(value=2), Replaced(value=3)\n",
    );
    let publication = publication(&[("click.py", SOURCE)], None, &["click"]);
    let keyword_reads = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.from.as_str().ends_with("/make")
                && ["/Base/label", "/Options/value"]
                    .iter()
                    .any(|suffix| edge.to.as_str().ends_with(suffix))
        })
        .map(|edge| {
            let site = edge.occurrence.as_ref().expect("written keyword");
            let start = usize::try_from(site.range.start).expect("start");
            let end = usize::try_from(site.range.end).expect("end");
            (&SOURCE[start..end], edge.to.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        keyword_reads,
        [
            (
                "label",
                "rift://symbol/pypi/pypi.org/click/python/click/Base/label"
            ),
            (
                "value",
                "rift://symbol/pypi/pypi.org/click/python/click/Options/value"
            ),
        ],
    );
    assert!(publication.edges.iter().all(|edge| {
        !(edge.kinds == [EdgeKind::Reads]
            && edge.from.as_str().ends_with("/refused")
            && edge.to.as_str().ends_with("/value"))
    }));
    let remaining = publication
        .edges
        .iter()
        .filter(|edge| !(edge.kinds == [EdgeKind::Reads] && edge.from.as_str().ends_with("/make")))
        .cloned()
        .collect::<Vec<_>>();
    assert!(remaining.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/Existing/value")
    }));
    let capped = python_publication_with_limits(
        &[("click.py", SOURCE)],
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(remaining.len()),
    );
    assert_eq!(capped.declarations, publication.declarations);
    assert_eq!(capped.edges, remaining);
}

#[test]
fn package_python_attrs_keywords_reference_the_declaring_fields() {
    const MODEL: &str = concat!(
        "from attrs import define\n",
        "@define\n",
        "class Base:\n",
        "    label: str = ''\n",
        "@define(kw_only=True)\n",
        "class Options(Base):\n",
        "    value: int\n",
    );
    const CONSUMER: &str = concat!(
        "from .model import Options\n",
        "def make():\n",
        "    return Options(label='x', value=1)\n",
    );
    let files = [("click/model.py", MODEL), ("click/consumer.py", CONSUMER)];
    let complete = publication(&files, None, &["click"]);
    for (owner, keyword) in [("/Base/label", "label"), ("/Options/value", "value")] {
        let start = CONSUMER.find(keyword).expect("written keyword");
        assert!(
            complete.edges.iter().any(|edge| {
                edge.from.as_str().ends_with("/consumer/make")
                    && edge.to.as_str().ends_with(owner)
                    && edge.kinds == [EdgeKind::Reads]
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.range.start == u64::try_from(start).expect("start")
                            && site.range.end == u64::try_from(start + keyword.len()).expect("end")
                    })
            }),
            "{owner}"
        );
        assert!(
            complete
                .declarations
                .iter()
                .any(|decl| decl.symbol.as_str().ends_with(owner))
        );
    }
    let earlier = complete
        .edges
        .iter()
        .filter(|edge| {
            !(edge.kinds == [EdgeKind::Reads] && edge.from.as_str().ends_with("/consumer/make"))
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(earlier.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/consumer/make")
            && edge.to.as_str().ends_with("/model/Options")
    }));
    let capped = python_publication_with_limits(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(earlier.len()),
    );
    assert_eq!(capped.declarations, complete.declarations);
    assert_eq!(capped.edges, earlier);
}

#[test]
fn package_python_attrs_keywords_require_a_proved_initializer() {
    const SOURCE: &str = concat!(
        "from attrs import define\n",
        "from typing import ClassVar\n",
        "@define(init=False)\n",
        "class Disabled:\n",
        "    value: int\n",
        "@define\n",
        "class Overrides:\n",
        "    value: int\n",
        "    def __init__(self, value): pass\n",
        "@define\n",
        "class ClassOnly:\n",
        "    value: ClassVar[int] = 0\n",
        "def define(cls): return cls\n",
        "@define\n",
        "class Shadowed:\n",
        "    value: int\n",
        "def make():\n",
        "    return Disabled(value=1), Overrides(value=1), ClassOnly(value=1), Shadowed(value=1)\n",
    );
    let result = publication(&[("click.py", SOURCE)], None, &["click"]);
    for owner in ["/Disabled", "/Overrides", "/ClassOnly", "/Shadowed"] {
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.from.as_str().ends_with("/make")
                    && edge.to.as_str().ends_with(owner)
            }),
            "{owner}"
        );
        let field = format!("{owner}/value");
        assert!(
            !result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.from.as_str().ends_with("/make")
                    && edge.to.as_str().ends_with(&field)
            }),
            "{field}"
        );
    }
}
