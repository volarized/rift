//! Python callees: a call with a target is no read of what it calls.

use super::{ReferenceRole, WORK_MAX, calls, resolution, source_of, target};

const SOURCE: &str = "import mod\nclass Base:\n    def m(self): pass\nclass Item(Base):\n    value = 1\n    def m(self): super().m()\n    def n(self): self.m()\n    @classmethod\n    def c(cls): cls.s()\n    @staticmethod\n    def s(): pass\n    @property\n    def p(self): pass\ndef on_instance():\n    item = Item()\n    item.m()\ndef on_class():\n    Item.s()\n    Item.m(Item())\ndef on_module(): mod.f()\n@mod.f\ndef decorated(): pass\ndef values(item: Item): return item.p, Item.value, mod.f, item.m, item.n().x\ndef unknown(item: Item): item.value()\n";

#[test]
fn test_python_established_member_callees_are_calls_alone() {
    let result = resolution(
        "python",
        &[("main.py", SOURCE), ("mod.py", "def f(x=None): pass\n")],
        &[],
        WORK_MAX,
    );
    let called = calls(&result, "main.py")
        .map(|call| source_of(call, SOURCE))
        .collect::<Vec<_>>();
    assert_eq!(
        called,
        [
            "super().m()",
            "self.m()",
            "cls.s()",
            "Item()",
            "item.m()",
            "Item.s()",
            "Item.m(Item())",
            "Item()",
            "mod.f()",
            "mod.f",
            "item.n()",
        ]
    );
    let reads = result
        .occurrences
        .iter()
        .filter(|access| access.role == ReferenceRole::Read && access.edge_kind.is_none())
        .collect::<Vec<_>>();
    let module = "rift://symbol/local/python/mod";
    let mut qualifier_sites = reads
        .iter()
        .filter(|access| target(access) == module)
        .map(|access| {
            assert_eq!(access.binding.unit().key().as_str(), "main.py");
            assert_eq!(source_of(access, SOURCE), "mod");
            let range = access.binding.range();
            (range.start(), range.end())
        })
        .collect::<Vec<_>>();
    qualifier_sites.sort_unstable();
    let expected_sites = SOURCE
        .match_indices("mod.f")
        .map(|(offset, _)| {
            let start = u64::try_from(offset).expect("qualifier offset");
            (start, start + 3)
        })
        .collect::<Vec<_>>();
    assert_eq!(qualifier_sites, expected_sites);
    let mut read = reads
        .into_iter()
        .filter(|access| target(access) != module)
        .map(|access| (source_of(access, SOURCE), target(access)))
        .collect::<Vec<_>>();
    read.sort_unstable();
    let member = |name| format!("rift://symbol/local/python/{name}");
    let expected = [
        ("Item", member("main/Item")),
        ("Item", member("main/Item")),
        ("Item.value", member("main/Item/value")),
        ("item.m", member("main/Item/m")),
        ("item.p", member("main/Item/p")),
        ("item.value", member("main/Item/value")),
        ("mod.f", member("mod/f")),
    ];
    let read = read
        .into_iter()
        .map(|(text, target)| (text, target.to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(read, expected);
}

#[test]
fn test_python_callees_of_a_dependency_are_calls_alone() {
    let source = "import os\ndef f(): os.getcwd()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(result.external.len(), 3, "{:?}", result.external);
    let read = result
        .external
        .iter()
        .find(|external| external.role == ReferenceRole::Read)
        .expect("written module qualifier");
    assert_eq!(
        read.target.as_str(),
        "rift://symbol/stdlib/cpython/python/os"
    );
    assert_eq!(
        (read.binding.range().start(), read.binding.range().end()),
        (19, 21)
    );
    assert!(result.external.iter().any(|call| {
        call.role == ReferenceRole::Call && call.target.as_str().ends_with("/os/getcwd")
    }));
}

#[test]
fn test_python_callees_read_through_another_name_keep_their_read() {
    let source = "class Item:\n    def m(self): pass\nclass Owner:\n    kind = Item\n    def make(self): return self.kind()\n    def take(self, items: list[Item]):\n        for item in items: print(item.m)\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let called = calls(&result, "main.py").map(target).collect::<Vec<_>>();
    assert_eq!(called, ["rift://symbol/local/python/main/Item"]);
    let read = result
        .occurrences
        .iter()
        .filter(|access| access.role == ReferenceRole::Read && access.edge_kind.is_none())
        .map(|access| (source_of(access, source), target(access)))
        .collect::<Vec<_>>();
    for (text, declaration) in [
        ("self.kind", "rift://symbol/local/python/main/Owner/kind"),
        ("item.m", "rift://symbol/local/python/main/Item/m"),
    ] {
        assert!(read.contains(&(text, declaration)), "{read:?}");
    }
    assert!(!read.contains(&("self.kind", "rift://symbol/local/python/main/Item")));
}
