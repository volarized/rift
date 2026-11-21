//! Python class names assigned again after their class statement.

use super::{WORK_MAX, labelled, resolution};

const BASES: &str = "class Base:\n    def connect(self) -> int: return 1\nclass Secure(Base):\n    def connect(self) -> int: return 2\n";

fn facts(sources: &[(&str, &str)]) -> Vec<(String, String, String)> {
    labelled(&resolution("python", sources, &[], WORK_MAX))
}

fn fact(label: &str, from: &str, to: &str) -> (String, String, String) {
    (
        label.to_owned(),
        format!("rift://symbol/local/python/{from}"),
        format!("rift://symbol/local/python/{to}"),
    )
}

fn sources_from(facts: &[(String, String, String)], from: &str) -> Vec<(String, String, String)> {
    let from = format!("rift://symbol/local/python/{from}");
    facts
        .iter()
        .filter(|(_, source, _)| source.starts_with(&from))
        .cloned()
        .collect()
}

#[test]
fn test_python_class_rebinding_keeps_wrapped_and_same_class_values() {
    let connection = format!(
        "def wrap(cls): return cls\n{BASES}Secure = wrap(Secure)\nclass Same(Base):\n    def connect(self) -> int: return 3\nSame = Same\n"
    );
    let socks = "from .connection import Secure, Same\nclass Tunnel(Secure):\n    def connect(self) -> int: return super().connect()\nclass Mirror(Same):\n    def connect(self) -> int: return super().connect()\ndef build() -> int: return Secure().connect()\n";
    let observed = facts(&[
        ("pkg/__init__.py", ""),
        ("pkg/connection.py", &connection),
        ("pkg/socks.py", socks),
    ]);
    for expected in [
        fact("Extends", "pkg/socks/Tunnel", "pkg/connection/Secure"),
        fact("Extends", "pkg/socks/Mirror", "pkg/connection/Same"),
        fact(
            "Overrides",
            "pkg/socks/Tunnel/connect",
            "pkg/connection/Secure/connect",
        ),
        fact(
            "call",
            "pkg/socks/Tunnel/connect",
            "pkg/connection/Secure/connect",
        ),
        fact(
            "call",
            "pkg/socks/Mirror/connect",
            "pkg/connection/Same/connect",
        ),
        fact("call", "pkg/socks/build", "pkg/connection/Secure"),
        fact("call", "pkg/socks/build", "pkg/connection/Secure/connect"),
    ] {
        assert!(observed.contains(&expected), "{expected:?}\n{observed:#?}");
    }
}

#[test]
fn test_python_class_rebinding_resolves_module_reads_before_another_value() {
    let connection = format!(
        "try:\n    import ssl\nexcept ImportError:\n    ssl = None\n{BASES}class Early(Secure):\n    def connect(self) -> int: return super().connect()\nclass Dummy:\n    pass\nif not ssl:\n    Secure = Dummy\ndef later() -> int: return Secure().connect()\n"
    );
    let socks = "from .connection import Secure\nclass Tunnel(Secure):\n    def connect(self) -> int: return super().connect()\ndef build() -> int: return Secure().connect()\n";
    let observed = facts(&[
        ("pkg/__init__.py", ""),
        ("pkg/connection.py", &connection),
        ("pkg/socks.py", socks),
    ]);
    for expected in [
        fact("Extends", "pkg/connection/Early", "pkg/connection/Secure"),
        fact(
            "Overrides",
            "pkg/connection/Early/connect",
            "pkg/connection/Secure/connect",
        ),
        fact(
            "call",
            "pkg/connection/Early/connect",
            "pkg/connection/Secure/connect",
        ),
    ] {
        assert!(observed.contains(&expected), "{expected:?}\n{observed:#?}");
    }
    // Uses after another value is assigned stay unresolved: imports, later class statements
    // and function bodies.
    for refused in [
        "pkg/socks/Tunnel",
        "pkg/socks/build",
        "pkg/connection/later",
    ] {
        let found = sources_from(&observed, refused);
        assert!(
            found
                .iter()
                .all(|(_, _, target)| !target.contains("/connection/Secure")),
            "{refused}: {found:#?}"
        );
    }
}

#[test]
fn test_python_class_rebinding_refuses_other_values_and_competing_classes() {
    for connection in [
        format!("def wrap(cls): return cls\n{BASES}Secure = wrap(Base)\n"),
        format!("{BASES}Secure = Base\n"),
        format!("def make(): return 1\n{BASES}Secure = make()\n"),
        format!("{BASES}class Secure(Base):\n    def connect(self) -> int: return 4\n"),
    ] {
        let socks = "from .connection import Secure\nclass Tunnel(Secure):\n    def connect(self) -> int: return super().connect()\n";
        let observed = facts(&[
            ("pkg/__init__.py", ""),
            ("pkg/connection.py", &connection),
            ("pkg/socks.py", socks),
        ]);
        let found = sources_from(&observed, "pkg/socks/Tunnel");
        assert!(
            found
                .iter()
                .all(|(_, _, target)| !target.contains("/connection/")),
            "{connection}\n{found:#?}"
        );
    }
}

#[test]
fn test_python_module_attribute_writes_name_the_written_declaration() {
    let patch = "import pkg.connection\nclass Replacement:\n    def connect(self) -> int: return 3\ndef inject() -> None:\n    pkg.connection.Base = Replacement\n    pkg.connection.missing = Replacement\n";
    let socks = "from .connection import Base\nclass Plain(Base):\n    def connect(self) -> int: return super().connect()\n";
    let observed = facts(&[
        ("pkg/__init__.py", ""),
        ("pkg/connection.py", BASES),
        ("pkg/socks.py", socks),
        ("pkg/patch.py", patch),
    ]);
    let writes = sources_from(&observed, "pkg/patch/inject")
        .into_iter()
        .filter(|(label, _, _)| label == "Write")
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        [fact("Write", "pkg/patch/inject", "pkg/connection/Base")],
        "{observed:#?}"
    );
    // A class replaced at runtime from another module keeps its uses unresolved.
    let found = sources_from(&observed, "pkg/socks/Plain");
    assert!(
        found
            .iter()
            .all(|(_, _, target)| !target.contains("/connection/Base")),
        "{found:#?}"
    );
}
