use super::{WORK_MAX, calls, resolution, source_of, target};

#[test]
fn test_python_callable_aliases_resolve_functions_and_classes() {
    let source = "def target(): pass\nclass Item: pass\nalias = target\nsecond = alias\nKind = Item\ndef aliased(): return alias()\ndef chained(): return second()\ndef built(): return Kind()\n";
    let imported = "from main import alias\ndef use(): return alias()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("use.py", imported)],
        &[],
        WORK_MAX,
    );
    let observed = calls(&result, "main.py")
        .map(|edge| {
            (
                edge.edge_from.as_ref().map(rift_core::SymbolId::as_str),
                source_of(edge, source),
                target(edge),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        [
            (
                Some("rift://symbol/local/python/main/aliased"),
                "alias()",
                "rift://symbol/local/python/main/target"
            ),
            (
                Some("rift://symbol/local/python/main/chained"),
                "second()",
                "rift://symbol/local/python/main/target"
            ),
            (
                Some("rift://symbol/local/python/main/built"),
                "Kind()",
                "rift://symbol/local/python/main/Item"
            ),
        ],
        "{result:?}"
    );
    assert_eq!(
        calls(&result, "use.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/target"],
        "{result:?}"
    );
    assert!(!result.exhausted);
}

#[test]
fn test_python_callable_aliases_refuse_rebinding_branches_scopes_and_values() {
    for source in [
        "def target(): pass\nalias = target\nalias = unknown\ndef use(): return alias()\n",
        "def target(): pass\ndef other(): pass\nif enabled:\n    alias = target\nelse:\n    alias = other\ndef use(): return alias()\n",
        "def target(): pass\ndef local():\n    alias = target\ndef use(): return alias()\n",
        "value = 1\nalias = value\ndef use(): return alias()\n",
        "first = second\nsecond = first\nalias = first\ndef use(): return alias()\n",
        "def target(): pass\nalias = target\ndef use(alias): return alias()\n",
    ] {
        let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
        let unit = "rift://symbol/local/python/main/use";
        assert!(
            calls(&result, "main.py").all(|edge| edge
                .edge_from
                .as_ref()
                .is_none_or(|from| from.as_str() != unit)),
            "{source}\n{result:?}"
        );
    }
}

#[test]
fn test_python_class_bases_follow_aliases_and_refuse_other_values() {
    let source = "class Base: pass\nAlias = Base\nclass Child(Alias): pass\nclass Direct(Base): pass\ndef factory(): pass\nvalue = factory\nclass FromFunction(value): pass\nruntime = unknown.attr\nclass FromValue(runtime): pass\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let extends = result
        .occurrences
        .iter()
        .filter(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends))
        .map(|edge| {
            (
                edge.edge_from.as_ref().map(rift_core::SymbolId::as_str),
                source_of(edge, source),
                target(edge),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        extends,
        [
            (
                Some("rift://symbol/local/python/main/Child"),
                "Alias",
                "rift://symbol/local/python/main/Base"
            ),
            (
                Some("rift://symbol/local/python/main/Direct"),
                "Base",
                "rift://symbol/local/python/main/Base"
            ),
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_class_alias_bases_keep_inherited_members() {
    let source = "class Base:\n    def leaf(self): pass\nAlias = Base\nclass Child(Alias):\n    def use(self): return self.leaf()\nvalue = unknown\nclass Other(value):\n    def use(self): return self.leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py")
            .map(|edge| (
                edge.edge_from.as_ref().map(rift_core::SymbolId::as_str),
                target(edge)
            ))
            .collect::<Vec<_>>(),
        [(
            Some("rift://symbol/local/python/main/Child/use"),
            "rift://symbol/local/python/main/Base/leaf"
        )],
        "{result:?}"
    );
}
