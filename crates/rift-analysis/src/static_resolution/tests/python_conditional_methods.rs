//! Python methods defined under compound statements of a class body.

use super::{WORK_MAX, calls, resolution, source_of, target};

fn close_callers(source: &str) -> Vec<String> {
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    calls(&result, "main.py")
        .filter(|edge| {
            source_of(edge, source) == "self.close()"
                && target(edge) == "rift://symbol/local/python/main/Conn/close"
        })
        .filter_map(|edge| edge.edge_from.as_ref())
        .map(|from| {
            from.as_str()
                .trim_start_matches("rift://symbol/local/python/main/")
                .to_owned()
        })
        .collect()
}

#[test]
fn test_python_conditional_methods_keep_their_class_receiver() {
    let source = "import sys\nclass Conn:\n    def close(self) -> None:\n        pass\n    if sys.version_info < (3, 99):\n        def tunnel(self) -> None:\n            self.close()\n    else:\n        def tunnel(self) -> None:\n            pass\n    try:\n        import ssl\n    except ImportError:\n        def secure(self) -> None:\n            self.close()\n    else:\n        def secure(self) -> None:\n            pass\n    if sys.platform == 'win32':\n        if sys.version_info >= (3, 12):\n            @property\n            def nested(self) -> int:\n                self.close()\n                return 1\n    def plain(self) -> None:\n        self.close()\n";
    assert_eq!(
        close_callers(source),
        ["Conn/tunnel", "Conn/secure", "Conn/nested", "Conn/plain"]
    );
}

#[test]
fn test_python_conditional_methods_exclude_nested_functions_and_static_methods() {
    let source = "import sys\nclass Conn:\n    def close(self) -> None:\n        pass\n    if sys.version_info < (3, 99):\n        def outer(self) -> None:\n            def inner(self) -> None:\n                self.close()\n            inner(self)\n        @staticmethod\n        def detached(self) -> None:\n            self.close()\ndef close(self) -> None:\n    pass\nif sys.version_info < (3, 99):\n    def module_level(self) -> None:\n        self.close()\n";
    assert_eq!(close_callers(source), Vec::<String>::new());
}
