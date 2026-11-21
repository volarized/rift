use super::{WORK_MAX, calls, resolution, source_of, target};

#[test]
fn test_python_bare_decorators_call_their_resolved_targets() {
    let source = "import helpers\ndef decorate(fn=None): return fn\nclass Wrap:\n    def __init__(self, fn): pass\n@decorate\ndef bare(): return 1\n@decorate()\ndef called(): return 2\n@Wrap\n@decorate\ndef stacked(): pass\n@helpers.mark\nclass Dotted: pass\nclass Owner:\n    @decorate\n    def method(self): pass\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("helpers.py", "def mark(fn): return fn\n"),
        ],
        &[],
        WORK_MAX,
    );
    let observed = calls(&result, "main.py")
        .map(|edge| {
            (
                edge.edge_from
                    .as_ref()
                    .map(rift_core::SymbolId::as_str)
                    .and_then(|from| from.strip_prefix("rift://symbol/local/python/")),
                source_of(edge, source),
                target(edge).strip_prefix("rift://symbol/local/python/"),
            )
        })
        .collect::<Vec<_>>();
    for expected in [
        (Some("main/bare"), "decorate", Some("main/decorate")),
        (Some("main/called"), "decorate()", Some("main/decorate")),
        (Some("main/stacked"), "Wrap", Some("main/Wrap")),
        (Some("main/stacked"), "decorate", Some("main/decorate")),
        (Some("main/Dotted"), "helpers.mark", Some("helpers/mark")),
        (Some("main/Owner/method"), "decorate", Some("main/decorate")),
    ] {
        assert!(observed.contains(&expected), "{expected:?}\n{observed:?}");
    }
    assert_eq!(observed.len(), 6, "{observed:?}");
    assert!(!result.exhausted);
}

#[test]
fn test_python_bare_decorators_refuse_unbound_rebound_and_external_targets() {
    for source in [
        "def scope():\n    def decorate(fn): return fn\n@decorate\ndef bare(): pass\n",
        "def decorate(fn): return fn\ndecorate = unknown\n@decorate\ndef bare(): pass\n",
        "from external import decorate\n@decorate\ndef bare(): pass\n",
        "class Owner:\n    @property\n    def value(self): return 1\n    @value.setter\n    def value(self, item): pass\n    @staticmethod\n    def build(): pass\n",
    ] {
        let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
        assert_eq!(calls(&result, "main.py").count(), 0, "{source}\n{result:?}");
    }
}
