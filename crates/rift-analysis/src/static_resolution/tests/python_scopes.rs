//! Python scopes: which binding of a name a position reads.

use super::{WORK_MAX, calls, resolution, target};

const NAMES: &str = "def h(): pass\ndef o(): pass\n";

/// The calls of one module as `caller -> callee`.
fn called(body: &str) -> Vec<String> {
    let source = format!("{NAMES}{body}");
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
    assert!(!result.exhausted);
    let prefix = "rift://symbol/local/python/main";
    let name = |identity: &str| {
        identity
            .trim_start_matches(prefix)
            .trim_start_matches('/')
            .to_owned()
    };
    calls(&result, "main.py")
        .map(|call| {
            let from = call.edge_from.as_ref().expect("caller");
            format!("{} -> {}", name(from.as_str()), name(target(call)))
        })
        .collect()
}

#[test]
fn test_python_targets_outside_assignments_bind_local_names() {
    for body in [
        "def f(x):\n    with x as h: h()\n",
        "def f(x):\n    with x as (h, [o, *rest]): h(); o()\n",
        "def f(x):\n    with (x as h, x as o): h(); o()\n",
        "async def f(x):\n    async with x as h: h()\n",
        "def f():\n    try: pass\n    except Exception as h: h()\n",
        "def f():\n    try: pass\n    except* (ValueError, TypeError) as h: h()\n",
        "def f(v):\n    match v:\n        case h: h()\n",
        "def f(v):\n    match v:\n        case [h, *o]: h(); o()\n",
        "def f(v):\n    match v:\n        case {'k': h, **o}: h(); o()\n",
        "def f(v):\n    match v:\n        case int() as h: h()\n",
        "def f(v):\n    match v:\n        case list(h, items=o): h(); o()\n",
        "def f(v):\n    match v:\n        case (1 | 2) as h | h: h()\n",
        "def f(*h: int, **o: int): h(); o()\n",
        "def f():\n    del h\n    h()\n",
        "def f():\n    del (h, [o])\n    h(); o()\n",
        "def f():\n    type h = int\n    h()\n",
        "def f():\n    type h[T] = list[T]\n    h()\n",
    ] {
        assert_eq!(called(body), Vec::<String>::new(), "{body}");
    }
}

#[test]
fn test_python_names_beside_targets_read_the_module() {
    for (body, expected) in [
        (
            "def f(x):\n    with h() as x.o: o()\n",
            ["f -> h", "f -> o"],
        ),
        (
            "def f(x):\n    with h() as x[o()]: pass\n",
            ["f -> h", "f -> o"],
        ),
        (
            "def f(v):\n    match v:\n        case o.h: h(); o()\n",
            ["f -> h", "f -> o"],
        ),
        (
            "def f(v):\n    match v:\n        case h(o=1): h(); o()\n",
            ["f -> h", "f -> o"],
        ),
        (
            "def f(x):\n    del x.h, x[o]\n    h(); o()\n",
            ["f -> h", "f -> o"],
        ),
        (
            "def f():\n    global h\n    del h\n    h(); o()\n",
            ["f -> h", "f -> o"],
        ),
    ] {
        assert_eq!(called(body), expected, "{body}");
    }
}

#[test]
fn test_python_deleted_local_keeps_its_receiver() {
    let body = "class C:\n    def m(self): pass\ndef f():\n    value = C()\n    value.m()\n    del value\n";
    assert_eq!(called(body), ["f -> C", "f -> C/m"]);
}

#[test]
fn test_python_annotated_collected_parameters_hold_no_instance() {
    let body = "class C:\n    def m(self): pass\n    def n(*args: int): args.m()\ndef f(*args: C, **kwargs: C):\n    args.m()\n    kwargs.m()\n";
    assert_eq!(called(body), Vec::<String>::new());
}

#[test]
fn test_python_defaults_annotations_and_class_arguments_read_the_enclosing_scope() {
    for (body, expected) in [
        ("def f(h=h()): pass\n", vec!["f -> h"]),
        ("def f(o=1, h=o()): pass\n", vec!["f -> o"]),
        ("value = lambda h=h(): 0\n", vec![" -> h"]),
        ("value = lambda h=1: h()\n", vec![]),
        ("def f():\n    h = 1\n    def g(o=h()): pass\n", vec![]),
        (
            "class C:\n    def m(self):\n        def g(h=h()): pass\n    h = 1\n",
            vec!["C/m/g -> h"],
        ),
        (
            "class C:\n    h = 1\n    def m(self, value=h()): pass\n",
            vec![],
        ),
        ("class C(metaclass=h()):\n    h = 1\n", vec!["C -> h"]),
        ("def f[h](value=h()): pass\n", vec![]),
        ("class C[h](metaclass=h()): pass\n", vec![]),
        ("class C[h]:\n    def m(self): h()\n", vec![]),
    ] {
        assert_eq!(called(body), expected, "{body}");
    }
}

#[test]
fn test_python_class_scopes_are_read_in_their_own_code_only() {
    for (body, expected) in [
        ("class C:\n    h = 1\n    value = h()\n", vec![]),
        (
            "class C:\n    h = 1\n    def m(self): h()\n",
            vec!["C/m -> h"],
        ),
        (
            "class C:\n    h = 1\n    value = [h() for item in (1,)]\n",
            vec!["C -> h"],
        ),
        (
            "class C:\n    h = 1\n    value = [item for item in h()]\n",
            vec![],
        ),
        (
            "class C:\n    h = 1\n    value = lambda self: h()\n",
            vec!["C -> h"],
        ),
        (
            "class C:\n    h = 1\n    value = lambda self=h(): 0\n",
            vec![],
        ),
        (
            "def f():\n    class C:\n        h = 1\n        value = h()\n",
            vec![],
        ),
        (
            "def f():\n    class C:\n        h = 1\n        def m(self, value=h()):\n            def g(value=h()): pass\n            h()\n",
            vec!["f/C/m/g -> h", "f/C/m -> h"],
        ),
        (
            "class C:\n    h = 1\n    class D:\n        value = h()\n",
            vec!["C/D -> h"],
        ),
    ] {
        assert_eq!(called(body), expected, "{body}");
    }
}

#[test]
fn test_python_annotations_read_past_the_parameters() {
    let body = "class C:\n    def m(self): pass\ndef f(C: C, o: C): o.m()\n";
    assert_eq!(called(body), ["f -> C/m"]);
}

#[test]
fn test_python_external_names_follow_the_same_scopes() {
    let source = "import re\nclass C:\n    re = 1\n    len = 1\n    def m(self, re=re.compile('')): return len('')\n    def n(self): re.escape('')\ndef f(len=len('')): pass\ndef g():\n    with f() as re: re.escape('')\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let called = result
        .external
        .iter()
        .filter(|external| external.role == super::ReferenceRole::Call)
        .map(|external| external.target.as_str().rsplit("/python/").next())
        .collect::<Vec<_>>();
    assert_eq!(
        called,
        [
            Some("builtins/len"),
            Some("re/escape"),
            Some("builtins/len")
        ]
    );
}
