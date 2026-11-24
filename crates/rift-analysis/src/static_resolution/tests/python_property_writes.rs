//! Python properties written through `self` before their class statement member.

use rift_protocol::read::EdgeKind;

use super::{WORK_MAX, calls, resolution, target};

fn overrides(source: &str) -> Vec<(String, String)> {
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    result
        .occurrences
        .iter()
        .filter(|edge| edge.edge_kind == Some(EdgeKind::Overrides))
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
fn test_python_property_written_in_the_initializer_keeps_its_overrides() {
    let observed = overrides(
        "class Base:\n    def __init__(self, retries: int) -> None:\n        self.retries = retries\n    @property\n    def retries(self) -> int:\n        return self._retries\n    @retries.setter\n    def retries(self, value: int) -> None:\n        self._retries = value\nclass Child(Base):\n    @property\n    def retries(self) -> int:\n        return 1\n",
    );
    assert!(
        observed.contains(&(
            "rift://symbol/local/python/main/Child/retries".to_owned(),
            "rift://symbol/local/python/main/Base/retries".to_owned()
        )),
        "{observed:?}"
    );
}

#[test]
fn test_python_instance_attributes_without_a_property_do_not_override() {
    let observed = overrides(
        "class Base:\n    def __init__(self, retries: int) -> None:\n        self.retries = retries\nclass Child(Base):\n    @property\n    def retries(self) -> int:\n        return 1\nclass Mixed:\n    def __init__(self) -> None:\n        self.value = 1\n    value = 2\n    @property\n    def value(self) -> int:\n        return 3\nclass Leaf(Mixed):\n    @property\n    def value(self) -> int:\n        return 4\n",
    );
    assert_eq!(observed, Vec::<(String, String)>::new());
}

fn overridden(source: &str) -> Vec<(String, String)> {
    let prefix = "rift://symbol/local/python/main/";
    overrides(source)
        .into_iter()
        .map(|(from, to)| {
            (
                from.trim_start_matches(prefix).to_owned(),
                to.trim_start_matches(prefix).to_owned(),
            )
        })
        .collect()
}

#[test]
fn test_python_initializer_writes_of_a_subclass_keep_the_inherited_slot() {
    let property = "class Base:\n    @property\n    def request(self) -> int: return 1\n    @request.setter\n    def request(self, value: int) -> None: pass\n    def close(self): pass\nclass Middle(Base):\n    def __init__(self, request: int) -> None:\n        self.request = request\n        self.close = None\nclass Leaf(Middle):\n    @property\n    def request(self) -> int: return 2\n    def close(self): pass\n";
    assert_eq!(
        overridden(property),
        [
            ("Leaf/request".to_owned(), "Base/request".to_owned()),
            ("Leaf/close".to_owned(), "Base/close".to_owned()),
        ]
    );
}

#[test]
fn test_python_property_getters_and_annotations_share_one_slot() {
    let observed = overridden(
        "import abc\nclass Base:\n    value: int\n    def __init__(self) -> None:\n        self.value = 1\n        self.kept = 1\n        self.late = 1\n    @property\n    def value(self) -> int: return 1\n    @property\n    def kept(self) -> int: return 1\n    @kept.getter\n    def kept(self) -> int: return 2\n    @abc.abstractproperty\n    def late(self) -> int: return 3\nclass Child(Base):\n    @property\n    def value(self) -> int: return 4\n    @property\n    def kept(self) -> int: return 5\n    @property\n    def late(self) -> int: return 6\n",
    );
    assert_eq!(
        observed,
        [
            ("Child/value".to_owned(), "Base/value".to_owned()),
            ("Child/kept".to_owned(), "Base/kept".to_owned()),
            ("Child/late".to_owned(), "Base/late".to_owned()),
        ]
    );
}

#[test]
fn test_python_properties_with_setters_supply_receivers() {
    let source = "class Item:\n    def read(self): pass\nclass Other:\n    def read(self): pass\nclass Owner:\n    def __init__(self, item: Item) -> None:\n        self.item = item\n    @property\n    def item(self) -> Item: return Item()\n    @item.setter\n    def item(self, value: Item) -> None: pass\n    @item.deleter\n    def item(self) -> None: pass\n    def use(self): return self.item.read()\n    @property\n    def mixed(self) -> Item: return Item()\n    mixed = Other()\n    def refused(self): return self.mixed.read()\ndef use(owner: Owner): return owner.item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let callers = calls(&result, "main.py")
        .filter(|edge| target(edge).ends_with("/Item/read"))
        .filter_map(|edge| edge.edge_from.as_ref())
        .map(|from| from.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        callers,
        [
            "rift://symbol/local/python/main/Owner/use",
            "rift://symbol/local/python/main/use"
        ]
    );
}

#[test]
fn test_python_property_chains_of_a_response_hierarchy_keep_every_override() {
    // `url` is written by the setter of another property and declared before it.
    let observed = overridden(
        "class Base:\n    def __init__(self, retries) -> None:\n        self.retries = retries\n    @property\n    def url(self): raise NotImplementedError()\n    @url.setter\n    def url(self, url): raise NotImplementedError()\n    @property\n    def retries(self): return self._retries\n    @retries.setter\n    def retries(self, retries):\n        self.url = retries.location\n        self._retries = retries\nclass Wrapper(Base):\n    def __init__(self, url) -> None:\n        self._url = url\n        super().__init__(None)\n    @property\n    def url(self): return self._url\n    @url.setter\n    def url(self, url): self._url = url\n    @property\n    def retries(self): return self._retries\n    @retries.setter\n    def retries(self, retries): self._retries = retries\n",
    );
    for pair in [
        ("Wrapper/url".to_owned(), "Base/url".to_owned()),
        ("Wrapper/retries".to_owned(), "Base/retries".to_owned()),
    ] {
        assert!(observed.contains(&pair), "{pair:?}: {observed:?}");
    }
}

#[test]
fn test_python_properties_redefined_by_another_getter_supply_no_receiver() {
    for body in [
        "    @property\n    def item(self) -> Item: return Item()\n    @property\n    def item(self) -> Other: return Other()\n",
        "    @property\n    def item(self) -> Item: return Item()\n    @item.getter\n    def item(self) -> Other: return Other()\n",
        "    @property\n    def item(self) -> Item: return Item()\n    def item(self) -> Other: return Other()\n",
        "    @property\n    def item(self) -> Item: return Item()\n    item: Other = Other()\n",
    ] {
        let source = format!(
            "class Item:\n    def read(self): pass\nclass Other:\n    def read(self): pass\nclass Owner:\n{body}    def use(self): return self.item.read()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert_eq!(
            calls(&result, "main.py")
                .filter(|edge| target(edge).ends_with("/read"))
                .count(),
            0,
            "{body}"
        );
    }
}

#[test]
fn test_python_attributes_assigned_two_classes_supply_no_receiver() {
    for (body, expected) in [
        (
            "        self.item = Item()\n        self.item = Other()\n",
            0,
        ),
        (
            "        self.item = Item()\n        if flag: self.item = Item()\n",
            1,
        ),
        ("        self.item = Item()\n        self.item = None\n", 1),
        ("        self.item = Item()\n        self.item = flag\n", 1),
        (
            "        self.item: Item = Item()\n        self.item = Other()\n",
            1,
        ),
    ] {
        let source = format!(
            "class Item:\n    def read(self): pass\nclass Other:\n    def read(self): pass\nclass Owner:\n    def __init__(self, flag):\n{body}    def use(self): return self.item.read()\n"
        );
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        let targets = calls(&result, "main.py")
            .map(target)
            .filter(|target| target.ends_with("/read"))
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec!["rift://symbol/local/python/main/Item/read"; expected],
            "{body}"
        );
    }
}

#[test]
fn test_python_super_reads_no_instance_attribute() {
    let source = "class Base:\n    def __init__(self, leaf):\n        self.leaf = leaf\n        self.close = leaf\n    def close(self): pass\nclass Child(Base):\n    def use(self):\n        super().leaf()\n        super().close()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let targets = calls(&result, "main.py")
        .filter(|edge| {
            edge.edge_from
                .as_ref()
                .is_some_and(|from| from.as_str().ends_with("/Child/use"))
        })
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(targets, ["rift://symbol/local/python/main/Base/close"]);
}

#[test]
fn test_python_annotations_continued_on_the_next_line_keep_their_value() {
    let source = "class Item:\n    def read(self): pass\nclass Other:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: return Item()\n    item: Other \\\n        = Other()\n    def use(self): return self.item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py")
            .filter(|edge| target(edge).ends_with("/Item/read"))
            .count(),
        0
    );
}
