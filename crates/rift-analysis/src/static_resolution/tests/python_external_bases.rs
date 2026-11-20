use rift_protocol::read::EdgeKind;

use super::{WORK_MAX, calls, resolution, source_of, target};

const EXTERNAL: &str = "from external import ExternalError\n";

/// Distinct (source, target) pairs of one edge kind.
type Pairs = Vec<(String, String)>;

fn facts(source: &str) -> (Pairs, Pairs) {
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("other.py", "from json import JSONDecodeError\n"),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.py")
        .map(|edge| (source_of(edge, source).to_owned(), target(edge).to_owned()))
        .collect();
    let overrides = result
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
        .collect();
    (calls, overrides)
}

#[test]
fn test_python_external_bases_keep_members_found_before_them() {
    for import in [
        EXTERNAL,
        "from other import JSONDecodeError as ExternalError\n",
    ] {
        let source = format!(
            "{import}class Base(IOError):\n    def __init__(self): pass\n    def leaf(self): pass\nclass Middle(Base): pass\nclass Child(Middle, ExternalError):\n    def __init__(self): super().__init__()\n    def use(self): return self.leaf()\n"
        );
        let (calls, overrides) = facts(&source);
        for call in [
            (
                "super().__init__()",
                "rift://symbol/local/python/main/Base/__init__",
            ),
            ("self.leaf()", "rift://symbol/local/python/main/Base/leaf"),
        ] {
            assert!(
                calls.contains(&(call.0.to_owned(), call.1.to_owned())),
                "{call:?}\n{calls:?}"
            );
        }
        assert!(
            overrides.contains(&(
                "rift://symbol/local/python/main/Child/__init__".to_owned(),
                "rift://symbol/local/python/main/Base/__init__".to_owned()
            )),
            "{overrides:?}"
        );
    }
}

#[test]
fn test_python_external_bases_refuse_members_after_them_and_unknown_bases() {
    for source in [
        format!(
            "{EXTERNAL}class Middle:\n    def leaf(self): pass\nclass Child(ExternalError, Middle):\n    def use(self): return self.leaf()\n"
        ),
        "class Middle:\n    def leaf(self): pass\nvalue = make()\nclass Child(value, Middle):\n    def use(self): return self.leaf()\n".to_owned(),
        "IOError = make()\nclass Base(IOError): pass\nclass Middle:\n    def leaf(self): pass\nclass Child(Base, Middle):\n    def use(self): return self.leaf()\n".to_owned(),
    ] {
        let (calls, _) = facts(&source);
        assert!(
            calls
                .iter()
                .all(|(_, target)| target != "rift://symbol/local/python/main/Middle/leaf"),
            "{source}\n{calls:?}"
        );
    }
}
