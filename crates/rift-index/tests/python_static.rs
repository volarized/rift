//! Python static facts retain their source and resolved identities.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};

fn build(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceIndex) {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[project]\nname = 'probe'\nversion = '0.1.0'\n",
    )
    .expect("project metadata");
    for (path, source) in files {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("source directory");
        std::fs::write(path, source).expect("source");
    }
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("static index");
    (directory, index)
}

#[test]
fn test_python_reexports_keep_the_written_module_and_terminal_import() {
    let source = "from __future__ import annotations\nimport typing\n\nif typing.TYPE_CHECKING:\n    from .barrel import Settings as Selected, Other\n\ndef enabled(record: Selected) -> bool:\n    return record.enabled\n";
    let refused = "from .shadowed import Settings as Shadowed\nfrom .cycle_a import Settings as Cyclic\nfrom .ambiguous import Settings as Ambiguous\n";
    let (_directory, index) = build(&[
        ("pkg/__init__.py", ""),
        (
            "pkg/model.py",
            "class Settings:\n    enabled: bool = True\nclass Other: pass\n",
        ),
        (
            "pkg/barrel.py",
            "from .model import Settings as Settings, Other as Other\n",
        ),
        ("pkg/use.py", source),
        ("pkg/direct.py", "from .model import Settings\n"),
        (
            "pkg/shadowed.py",
            "from .model import Settings as Settings\nSettings = []\n",
        ),
        (
            "pkg/cycle_a.py",
            "from .cycle_b import Settings as Settings\n",
        ),
        (
            "pkg/cycle_b.py",
            "from .cycle_a import Settings as Settings\n",
        ),
        (
            "pkg/ambiguous.py",
            "from .model import Settings as Settings\n",
        ),
        (
            "pkg/ambiguous/__init__.py",
            "from ..model import Settings as Settings\n",
        ),
        ("pkg/refused.py", refused),
    ]);
    let prefix = "rift://symbol/local/python/pkg/";
    for (kind, owner, target, spelling) in [
        ("imports", "use", "barrel", ".barrel"),
        ("imports", "use", "model/Settings", "Selected"),
        ("imports", "use", "model/Other", "Other"),
        ("has_type", "use/enabled", "model/Settings", "Selected"),
        (
            "reads",
            "use/enabled",
            "model/Settings/enabled",
            "record.enabled",
        ),
    ] {
        let from = format!("{prefix}{owner}");
        let to = format!("{prefix}{target}");
        let mut matches = index.edges().edges().filter(|edge| {
            edge.kind().as_ref() == kind && edge.from().as_str() == from && edge.to().as_str() == to
        });
        let edge = matches.next().expect("stored authored edge");
        assert!(matches.next().is_none());
        let binding = edge.occurrence();
        assert_eq!(binding.unit().key().as_str(), "pkg/use.py");
        let start = usize::try_from(binding.range().start()).expect("source start");
        let end = usize::try_from(binding.range().end()).expect("source end");
        assert_eq!(&source[start..end], spelling);
    }
    let mut direct = index.edges().edges().filter(|edge| {
        edge.kind().as_ref() == "imports"
            && edge.occurrence().unit().key().as_str() == "pkg/direct.py"
    });
    assert_eq!(
        direct.next().expect("stored direct import").to().as_str(),
        format!("{prefix}model/Settings")
    );
    assert!(direct.next().is_none());
    for edge in index.edges().edges().filter(|edge| {
        edge.kind().as_ref() == "imports"
            && edge.occurrence().unit().key().as_str() == "pkg/refused.py"
    }) {
        let range = edge.occurrence().range();
        let start = usize::try_from(range.start()).expect("source start");
        let end = usize::try_from(range.end()).expect("source end");
        assert!(![".shadowed", ".cycle_a", ".ambiguous"].contains(&&refused[start..end]));
    }
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_generic_mro_stores_the_ordered_local_target() {
    let source = "from typing import Generic, TypeVar\nT = TypeVar('T')\nclass Root:\n    def leaf(self): return 1\nclass Left(Generic[T], Root): pass\nclass Right(Generic[T]):\n    def leaf(self): return 2\nclass Child(Left[T], Right[T]):\n    def use(self): return self.leaf()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    let calls = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind().as_ref() == "calls"
                && edge.from().as_str() == "rift://symbol/local/python/main/Child/use"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].to().as_str(),
        "rift://symbol/local/python/main/Right/leaf"
    );
    let binding = calls[0].occurrence();
    let start = usize::try_from(binding.range().start()).expect("start");
    let end = usize::try_from(binding.range().end()).expect("end");
    assert_eq!(&source[start..end], "self.leaf()");
}

#[test]
fn test_python_generic_ancestry_stores_inherited_calls_and_overrides() {
    let source = "import typing as t\nT = t.TypeVar('T')\nclass Base(t.Generic[T]):\n    def leaf(self): pass\n    def report(self): pass\nclass Middle(Base[T], t.Generic[T]): pass\nclass Child(Middle[T], t.Generic[T]):\n    def report(self): return super().report()\n    def use(self): return self.leaf()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (kind, from, to, spelling) in [
        ("calls", "Child/report", "Base/report", "super().report()"),
        ("calls", "Child/use", "Base/leaf", "self.leaf()"),
        ("overrides", "Child/report", "Base/report", "report"),
    ] {
        let edge = index
            .edges()
            .edges()
            .find(|edge| {
                edge.kind().as_ref() == kind
                    && edge.from().as_str() == format!("rift://symbol/local/python/main/{from}")
                    && edge.to().as_str() == format!("rift://symbol/local/python/main/{to}")
            })
            .expect("stored inherited member");
        let binding = edge.occurrence();
        let start = usize::try_from(binding.range().start()).expect("start");
        let end = usize::try_from(binding.range().end()).expect("end");
        assert_eq!(&source[start..end], spelling);
        assert_eq!(binding.unit().key().as_str(), "main.py");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_overloads_store_implementation_calls_and_returned_receivers() {
    let source = "from typing import overload\nclass Item:\n    def leaf(self): pass\n@overload\ndef build(value: int) -> Item: ...\n@overload\ndef build(value: str) -> Item: ...\ndef build(value) -> Item: return Item()\ndef use():\n    item = build(1)\n    item.leaf()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (target, spelling) in [("build", "build(1)"), ("Item/leaf", "item.leaf()")] {
        let edge = index
            .edges()
            .edges()
            .find(|edge| {
                edge.kind().as_ref() == "calls"
                    && edge.from().as_str() == "rift://symbol/local/python/main/use"
                    && edge.to().as_str() == format!("rift://symbol/local/python/main/{target}")
            })
            .expect("stored resolved call");
        let range = edge.occurrence().range();
        let start = usize::try_from(range.start()).expect("call start");
        let end = usize::try_from(range.end()).expect("call end");
        assert_eq!(&source[start..end], spelling);
        assert_eq!(edge.occurrence().unit().key().as_str(), "main.py");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_property_returns_store_calls_and_property_reads() {
    let source = "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: return Item()\n    def use(self): return self.item.read()\ndef use(owner: Owner): return owner.item.read()\ndef invalid(): return Owner.item.read()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    let calls = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind().as_ref() == "calls"
                && edge.to().as_str() == "rift://symbol/local/python/main/Item/read"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "{:?}", index.edges());
    for edge in calls {
        assert!(
            [
                "rift://symbol/local/python/main/Owner/use",
                "rift://symbol/local/python/main/use"
            ]
            .contains(&edge.from().as_str())
        );
        let binding = edge.occurrence();
        assert_eq!(binding.unit().key().as_str(), "main.py");
        let start = usize::try_from(binding.range().start()).expect("source start");
        let end = usize::try_from(binding.range().end()).expect("source end");
        assert!(["self.item.read()", "owner.item.read()"].contains(&&source[start..end]));
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind().as_ref() == "reads"
                && edge.to().as_str() == "rift://symbol/local/python/main/Owner/item")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_wildcard_import_stores_the_module_and_explicit_alias() {
    let source = "from .leaf import *\ndef use(): return leaf()\n";
    let (_directory, index) = build(&[
        ("pkg/__init__.py", source),
        ("pkg/leaf.py", "value = 1\ndef leaf(): pass\n"),
        ("bridge.py", "from pkg.leaf import value as value\n"),
    ]);
    let imported = index
        .edges()
        .edges()
        .find(|edge| {
            edge.kind().as_ref() == "imports"
                && edge.from().as_str() == "rift://symbol/local/python/pkg"
                && edge.to().as_str() == "rift://symbol/local/python/pkg/leaf"
        })
        .expect("stored wildcard module import");
    let binding = imported.occurrence();
    assert_eq!(binding.unit().key().as_str(), "pkg/__init__.py");
    let start = usize::try_from(binding.range().start()).expect("source start");
    let end = usize::try_from(binding.range().end()).expect("source end");
    assert_eq!(&source[start..end], "*");
    for (kind, from) in [
        ("imports", "rift://symbol/local/python/bridge"),
        ("aliases", "rift://symbol/local/python/bridge/value"),
    ] {
        assert!(
            index.edges().edges().any(|edge| {
                edge.kind().as_ref() == kind
                    && edge.from().as_str() == from
                    && edge.to().as_str() == "rift://symbol/local/python/pkg/leaf/value"
            }),
            "{kind}: {:?}",
            index.edges()
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| edge.kind().as_ref() != "calls")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_overloads_do_not_store_replaced_slots() {
    let source = "from typing import overload\nclass Owner:\n    @overload\n    def build(self, value: int): ...\n    def build(self, value): pass\nOwner.build = unknown\ndef use(owner: Owner): owner.build(1)\n";
    let (_directory, index) = build(&[("main.py", source)]);
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| edge.kind().as_ref() != "calls"
                || edge.to().as_str() != "rift://symbol/local/python/main/Owner/build")
    );
}

#[test]
fn test_python_package_self_imports_store_submodule_imports_and_calls() {
    let package = "from . import leaf\n";
    let sibling = "from . import leaf\ndef use(): return leaf.target()\n";
    let (_directory, index) = build(&[
        ("pkg/__init__.py", package),
        ("pkg/leaf.py", "def target(): pass\n"),
        ("pkg/sibling.py", sibling),
    ]);
    for (kind, from, to, path, source, spelling) in [
        (
            "imports",
            "pkg",
            "pkg/leaf",
            "pkg/__init__.py",
            package,
            "leaf",
        ),
        (
            "imports",
            "pkg/sibling",
            "pkg/leaf",
            "pkg/sibling.py",
            sibling,
            "leaf",
        ),
        (
            "calls",
            "pkg/sibling/use",
            "pkg/leaf/target",
            "pkg/sibling.py",
            sibling,
            "leaf.target()",
        ),
    ] {
        let edge = index
            .edges()
            .edges()
            .find(|edge| {
                edge.kind().as_ref() == kind
                    && edge.from().as_str() == format!("rift://symbol/local/python/{from}")
                    && edge.to().as_str() == format!("rift://symbol/local/python/{to}")
            })
            .unwrap_or_else(|| panic!("{kind} {from} -> {to}: {:?}", index.edges()));
        let binding = edge.occurrence();
        assert_eq!(binding.unit().key().as_str(), path);
        let start = usize::try_from(binding.range().start()).expect("start");
        let end = usize::try_from(binding.range().end()).expect("end");
        assert_eq!(&source[start..end], spelling);
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

fn stored(
    index: &WorkspaceIndex,
    source: &str,
    kind: &str,
    from: &str,
    to: &str,
) -> Option<String> {
    let edge = index.edges().edges().find(|edge| {
        edge.kind().as_ref() == kind
            && edge.from().as_str() == format!("rift://symbol/local/python/{from}")
            && edge.to().as_str() == format!("rift://symbol/local/python/{to}")
    })?;
    let range = edge.occurrence().range();
    let start = usize::try_from(range.start()).ok()?;
    let end = usize::try_from(range.end()).ok()?;
    Some(source[start..end].to_owned())
}

#[test]
fn test_python_callable_aliases_store_calls_and_class_bases() {
    let source = "def target(): pass\nclass Base:\n    def leaf(self): pass\nalias = target\nKind = Base\ndef aliased(): return alias()\ndef built(): return Kind()\nclass Child(Kind):\n    def use(self): return self.leaf()\nruntime = unknown.attr\nclass Other(runtime): pass\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (kind, from, to, spelling) in [
        ("calls", "main/aliased", "main/target", "alias()"),
        ("calls", "main/built", "main/Base", "Kind()"),
        ("extends", "main/Child", "main/Base", "Kind"),
        ("calls", "main/Child/use", "main/Base/leaf", "self.leaf()"),
    ] {
        assert_eq!(
            stored(&index, source, kind, from, to).as_deref(),
            Some(spelling),
            "{kind} {from} -> {to}: {:?}",
            index.edges()
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| edge.kind().as_ref() != "extends"
                || edge.from().as_str() != "rift://symbol/local/python/main/Other")
    );
    assert!(index.edges().is_complete());
}

#[test]
fn test_python_bare_decorators_store_calls() {
    let source = "def decorate(fn=None): return fn\n@decorate\ndef bare(): return 1\n@decorate()\ndef called(): return 2\nclass Owner:\n    @property\n    def value(self): return 1\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (from, spelling) in [("main/bare", "decorate"), ("main/called", "decorate()")] {
        assert_eq!(
            stored(&index, source, "calls", from, "main/decorate").as_deref(),
            Some(spelling),
            "{from}: {:?}",
            index.edges()
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind().as_ref() == "calls"
                && edge.to().as_str().starts_with("rift://symbol/local/"))
            .all(|edge| edge.to().as_str() == "rift://symbol/local/python/main/decorate")
    );
    // `@property` calls the standard-library builtin, never a local declaration (D23).
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind().as_ref() == "calls"
                && edge.to().as_str() == "rift://symbol/stdlib/cpython/python/builtins/property")
    );
    assert!(index.edges().is_complete());
}

#[test]
fn test_python_rebound_locals_and_property_setters_store_member_facts() {
    let source = "class Response:\n    @property\n    def elapsed(self): return 1\n    @elapsed.setter\n    def elapsed(self, value): pass\n    def close(self): pass\ndef fetch() -> Response: return Response()\ndef send(response: Response):\n    response.elapsed = 2\n    while response:\n        response.close()\n        response = fetch()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (kind, to, spelling) in [
        ("writes", "main/Response/elapsed", "response.elapsed"),
        ("calls", "main/Response/close", "response.close()"),
    ] {
        assert_eq!(
            stored(&index, source, kind, "main/send", to).as_deref(),
            Some(spelling),
            "{kind} -> {to}: {:?}",
            index.edges()
        );
    }
    assert!(index.edges().is_complete());
}

#[test]
fn test_python_reads_before_a_later_rebinding_store_member_facts() {
    let source = "class Item:\n    value = 0\n    def leaf(self): pass\ndef make() -> Item: return Item()\ndef use(items):\n    item = make()\n    item.value = 1\n    item.leaf()\n    item = items.pop()\n    return item\n";
    let (_directory, index) = build(&[("main.py", source)]);
    for (kind, to, spelling) in [
        ("writes", "main/Item/value", "item.value"),
        ("calls", "main/Item/leaf", "item.leaf()"),
    ] {
        assert_eq!(
            stored(&index, source, kind, "main/use", to).as_deref(),
            Some(spelling),
            "{kind} -> {to}: {:?}",
            index.edges()
        );
    }
    assert!(index.edges().is_complete());
}

#[test]
fn test_python_conditional_receivers_store_calls_before_and_after_a_none_reset() {
    let source = "class Item:\n    def start(self): pass\n    def stop(self): pass\ndef use(enabled):\n    first: Item | None = None\n    second: Item | None = None\n    if enabled:\n        first = Item()\n        second = Item()\n        try:\n            first.start()\n            second.start()\n        except OSError:\n            first = second = None\n            first.start()\n    if first is not None:\n        first.stop()\n    if second is not None:\n        second.stop()\n";
    let (_directory, index) = build(&[("main.py", source)]);
    let mut calls = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.kind().as_ref() == "calls"
                && edge.from().as_str() == "rift://symbol/local/python/main/use"
                && edge
                    .to()
                    .as_str()
                    .starts_with("rift://symbol/local/python/main/Item/")
        })
        .map(|edge| {
            let occurrence = edge.occurrence();
            assert_eq!(occurrence.unit().key().as_str(), "main.py");
            let start = usize::try_from(occurrence.range().start()).expect("start");
            let end = usize::try_from(occurrence.range().end()).expect("end");
            (start, end, edge.to().as_str())
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "first.start()",
            "rift://symbol/local/python/main/Item/start",
        ),
        (
            "second.start()",
            "rift://symbol/local/python/main/Item/start",
        ),
        ("first.stop()", "rift://symbol/local/python/main/Item/stop"),
        ("second.stop()", "rift://symbol/local/python/main/Item/stop"),
    ]
    .map(|(spelling, target)| {
        let start = source.find(spelling).expect("first written call");
        (start, start + spelling.len(), target)
    });
    calls.sort_unstable();
    assert_eq!(calls, expected);
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_python_rebound_classes_optional_receivers_and_conditional_methods_store_facts() {
    let connection = "def wrap(cls): return cls\nclass Base:\n    def connect(self) -> int: return 1\nclass Secure(Base):\n    def connect(self) -> int: return 2\nSecure = wrap(Secure)\n";
    let socks = "from .connection import Secure\nclass Tunnel(Secure):\n    def connect(self) -> int: return super().connect()\ndef build() -> int: return Secure().connect()\n";
    let patch = "import pkg.connection\nclass Replacement:\n    pass\ndef inject() -> None:\n    pkg.connection.Base = Replacement\n";
    let optional = "import typing as t\nclass Item:\n    def read(self) -> int: return 1\ndef use(item: t.Optional[Item]) -> int:\n    return item.read() if item is not None else 0\n";
    let conditional = "import sys\nclass Conn:\n    def close(self) -> None: pass\n    if sys.version_info < (3, 99):\n        def tunnel(self) -> None:\n            self.close()\n";
    let (_directory, index) = build(&[
        ("pkg/__init__.py", ""),
        ("pkg/connection.py", connection),
        ("pkg/socks.py", socks),
        ("pkg/patch.py", patch),
        ("pkg/optional.py", optional),
        ("pkg/conditional.py", conditional),
    ]);
    for (source, kind, from, to, spelling) in [
        (
            socks,
            "extends",
            "pkg/socks/Tunnel",
            "pkg/connection/Secure",
            "Secure",
        ),
        (
            socks,
            "calls",
            "pkg/socks/Tunnel/connect",
            "pkg/connection/Secure/connect",
            "super().connect()",
        ),
        (
            socks,
            "calls",
            "pkg/socks/build",
            "pkg/connection/Secure/connect",
            "Secure().connect()",
        ),
        (
            patch,
            "writes",
            "pkg/patch/inject",
            "pkg/connection/Base",
            "pkg.connection.Base",
        ),
        (
            optional,
            "calls",
            "pkg/optional/use",
            "pkg/optional/Item/read",
            "item.read()",
        ),
        (
            conditional,
            "calls",
            "pkg/conditional/Conn/tunnel",
            "pkg/conditional/Conn/close",
            "self.close()",
        ),
    ] {
        assert_eq!(
            stored(&index, source, kind, from, to).as_deref(),
            Some(spelling),
            "{kind} {from} -> {to}: {:?}",
            index.edges()
        );
    }
    assert!(index.edges().is_complete());
}

#[test]
fn test_python_declared_container_elements_store_calls_alone() {
    let probe = "class Item:\n    def leaf(self): return 1\ndef values() -> list[Item]: return []\ndef use():\n    for value in values():\n        value.leaf()\n";
    let unpacked = "class Item:\n    def leaf(self): return 1\nclass Other:\n    def leaf(self): return 2\ndef use(pairs: list[tuple[str, Item]]):\n    for name, item in pairs:\n        item.leaf()\ndef refused(pairs: list[tuple[str, int, Item]]):\n    for name, item in pairs:\n        item.leaf()\n";
    let (_directory, index) = build(&[("probe.py", probe), ("unpacked.py", unpacked)]);
    for (source, module, spelling) in [
        (probe, "probe", "value.leaf"),
        (unpacked, "unpacked", "item.leaf"),
    ] {
        let use_ = format!("{module}/use");
        let leaf = format!("{module}/Item/leaf");
        assert_eq!(
            stored(&index, source, "calls", &use_, &leaf),
            Some(format!("{spelling}()")),
            "{:?}",
            index.edges()
        );
        assert_eq!(stored(&index, source, "reads", &use_, &leaf), None);
    }
    assert!(index.edges().edges().all(|edge| {
        !edge.from().as_str().ends_with("/refused")
            || !["calls", "reads"].contains(&edge.kind().as_ref())
    }));
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| !edge.to().as_str().ends_with("Other/leaf")
                || edge.kind().as_ref() == "contains")
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}
