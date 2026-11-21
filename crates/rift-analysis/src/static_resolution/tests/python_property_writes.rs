//! Python properties written through `self` before their class statement member.

use rift_protocol::read::EdgeKind;

use super::{WORK_MAX, resolution, target};

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
