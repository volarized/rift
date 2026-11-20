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
