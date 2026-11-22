use super::{WORK_MAX, calls, resolution, source_of, target};

const CLASSES: &str = "class Item:\n    def leaf(self): pass\nclass Other:\n    def leaf(self): pass\ndef make() -> Item: return Item()\n";

fn leaf_targets(body: &str) -> Vec<(String, String)> {
    let source = format!("{CLASSES}{body}");
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
    calls(&result, "main.py")
        .filter(|edge| source_of(edge, &source).ends_with(".leaf()"))
        .map(|edge| {
            (
                edge.edge_from
                    .as_ref()
                    .map(|from| from.as_str().to_owned())
                    .unwrap_or_default(),
                target(edge).to_owned(),
            )
        })
        .collect()
}

#[test]
fn test_python_rebound_locals_keep_an_agreed_receiver_class() {
    let observed = leaf_targets(
        "def loop(item: Item):\n    while item:\n        item.leaf()\n        item = make()\ndef branch(flag):\n    if flag:\n        value = Item()\n    else:\n        value = make()\n    value.leaf()\n",
    );
    assert_eq!(
        observed,
        [
            (
                "rift://symbol/local/python/main/loop".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned()
            ),
            (
                "rift://symbol/local/python/main/branch".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned()
            ),
        ]
    );
}

#[test]
fn test_python_rebound_locals_refuse_competing_and_unknown_values() {
    for body in [
        "def mixed(flag):\n    if flag:\n        value = Item()\n    else:\n        value = Other()\n    value.leaf()\n",
        "def unknown(flag):\n    value = Item()\n    if flag:\n        value = factory()\n    value.leaf()\n",
        "def looped(item: Item):\n    for other in items:\n        item.leaf()\n        item = other\n",
        "def unpacked(item: Item):\n    item, rest = pair\n    item.leaf()\n",
    ] {
        assert_eq!(leaf_targets(body), [], "{body}");
    }
}

#[test]
fn test_python_class_object_values_construct_their_class() {
    let observed = leaf_targets(
        "class Factory:\n    item_class: type[Item] = Item\n    alias = Item\ndef build(factory: Factory, item: Item | None = None):\n    if item is None:\n        item = factory.item_class()\n    item.leaf()\ndef direct(factory: Factory):\n    made = factory.alias()\n    made.leaf()\n",
    );
    assert_eq!(
        observed,
        [
            (
                "rift://symbol/local/python/main/build".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned()
            ),
            (
                "rift://symbol/local/python/main/direct".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned()
            ),
        ]
    );
}

#[test]
fn test_python_class_object_values_refuse_instances_and_shadowed_type() {
    for body in [
        "class Factory:\n    item: Item = Item()\ndef build(factory: Factory):\n    made = factory.item()\n    made.leaf()\n",
        "type = make\nclass Factory:\n    item_class: type[Item] = Item\ndef build(factory: Factory):\n    made = factory.item_class()\n    made.leaf()\n",
        "class Factory:\n    item_class: type = Item\ndef build(factory: Factory):\n    made = factory.item_class()\n    made.leaf()\n",
    ] {
        assert_eq!(leaf_targets(body), [], "{body}");
    }
}

#[test]
fn test_python_reads_before_a_later_rebinding_keep_their_receiver() {
    let observed = leaf_targets(
        "def use(items):\n    item = make()\n    item.leaf()\n    item = items.pop()\n    return item\n",
    );
    assert_eq!(
        observed,
        [(
            "rift://symbol/local/python/main/use".to_owned(),
            "rift://symbol/local/python/main/Item/leaf".to_owned()
        )]
    );
}

#[test]
fn test_python_reads_before_a_later_rebinding_refuse_loops_and_delayed_bodies() {
    for body in [
        "def looped(items):\n    item = make()\n    for other in items:\n        item.leaf()\n        item = other\n",
        "def waiting(items):\n    item = make()\n    while items:\n        item.leaf()\n        item = items.pop()\n",
        "def delayed(items):\n    item = make()\n    later = lambda: item.leaf()\n    item = items.pop()\n    return later\n",
        "def generated(items):\n    item = make()\n    later = (item.leaf() for _ in items)\n    item = items.pop()\n    return later\n",
        "def outer(items):\n    item = make()\n    def inner(): return item.leaf()\n    item = items.pop()\n    return inner\n",
    ] {
        assert_eq!(leaf_targets(body), [], "{body}");
    }
}

#[test]
fn test_python_non_none_guards_keep_a_single_conditional_receiver_class() {
    let observed = leaf_targets(
        "cached: Item | None = None\nif enabled():\n    cached = Item()\nelse:\n    cached = None\ndef module_read():\n    if cached and ready():\n        return cached.leaf()\ndef local_read():\n    value = None\n    if enabled():\n        value = Item()\n    if value is not None:\n        return value.leaf()\n    value.leaf()\ndef chained():\n    value = Item()\n    if enabled():\n        value = other = None\n    if ready() and value is not None:\n        return value.leaf()\n",
    );
    assert_eq!(
        observed,
        [
            (
                "rift://symbol/local/python/main/module_read".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned(),
            ),
            (
                "rift://symbol/local/python/main/local_read".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned(),
            ),
            (
                "rift://symbol/local/python/main/chained".to_owned(),
                "rift://symbol/local/python/main/Item/leaf".to_owned(),
            ),
        ]
    );
}

#[test]
fn test_python_non_none_guards_refuse_competing_types_and_later_writes() {
    for body in [
        "def read(flag):\n    value = None\n    if flag:\n        value = Item()\n    else:\n        value = Other()\n    if value is not None:\n        value.leaf()\n",
        "def read(flag):\n    value = None\n    if flag:\n        value = Item()\n    if value is not None:\n        value = None\n        value.leaf()\n",
        "def read(flag):\n    value = None\n    if flag:\n        value = Item()\n    if value or flag:\n        value.leaf()\n",
        "def read(flag):\n    value = None\n    if flag:\n        value = Item()\n    if value is not None:\n        def deferred(): return value.leaf()\n",
        "def read(flag):\n    value = None\n    if flag:\n        value = Item()\n    if value is not None:\n        while flag:\n            value.leaf()\n            value = None\n",
        "value: Item | None = None\nif enabled():\n    value = Item()\nelse:\n    value = None\ndef change():\n    global value\n    value = Other()\ndef read():\n    if value is not None:\n        value.leaf()\n",
    ] {
        assert_eq!(leaf_targets(body), [], "{body}");
    }
}

#[test]
fn test_python_non_none_guards_refuse_module_slots_written_from_other_modules() {
    let source = format!(
        "{CLASSES}cached = None\nif enabled():\n    cached = Item()\ndef read():\n    if cached:\n        return cached.leaf()\n"
    );
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/main.py", &source),
            (
                "pkg/patch.py",
                "import pkg.main\ndef replace():\n    pkg.main.cached = object()\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert!(calls(&result, "pkg/main.py").all(|edge| !target(edge).ends_with("/Item/leaf")));
}

#[test]
fn test_python_receivers_resolve_before_a_later_none_reset() {
    let observed = leaf_targets(
        "def read(flag):\n    first: Item | None = None\n    second: Item | None = None\n    if flag:\n        first = Item()\n        second = Item()\n        try:\n            first.leaf()\n            second.leaf()\n        except OSError:\n            first = second = None\n",
    );
    let expected = (
        "rift://symbol/local/python/main/read".to_owned(),
        "rift://symbol/local/python/main/Item/leaf".to_owned(),
    );
    assert_eq!(observed, [expected.clone(), expected]);
}

#[test]
fn test_python_receivers_refuse_a_none_reset_that_may_have_run() {
    for body in [
        "def read(flag):\n    value = Item()\n    while flag:\n        value.leaf()\n        value = None\n",
        "def read(flag):\n    value = Item()\n    if flag:\n        value = None\n    value.leaf()\n",
        "def read():\n    value = Item()\n    later = lambda: value.leaf()\n    value = None\n    return later\n",
    ] {
        assert_eq!(leaf_targets(body), [], "{body}");
    }
}
