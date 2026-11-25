//! Captured reads and writes of plain Python names.

use rift_core::{ProjectPath, ReferenceRole};

use super::*;

/// The captured reads and writes of plain names, in source order.
fn captured(source: &str) -> Vec<(u64, &str, ReferenceRole)> {
    let path = ProjectPath::new("pkg/names.py").expect("valid fixture path");
    let document = PythonSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("Python fixture must parse");
    assert!(!document.has_errors(), "{source}");
    let facts = document.facts().static_facts().expect("static facts");
    let mut names = facts
        .references()
        .iter()
        .filter(|reference| reference.role != ReferenceRole::Type)
        .filter_map(|reference| {
            let start = usize::try_from(reference.target.start).expect("start");
            let end = usize::try_from(reference.target.end).expect("end");
            let name = &source[start..end];
            (!name.contains('.')).then_some((reference.target.start, name, reference.role))
        })
        .collect::<Vec<_>>();
    names.sort_by_key(|(start, _, _)| *start);
    names
}

/// The names read, in source order.
fn read(body: &str) -> String {
    let shared = "a = b = c = d = 1\n";
    let source = format!("{shared}{body}");
    let names = captured(&source);
    assert!(
        names
            .iter()
            .all(|(_, _, role)| *role == ReferenceRole::Read),
        "{body}: {names:?}"
    );
    let names = names.into_iter().map(|(_, name, _)| name);
    names.collect::<Vec<_>>().join(" ")
}

#[test]
fn test_python_names_in_load_position_are_read() {
    for (body, expected) in [
        ("x = a\n", "a"),
        ("x: int = a\n", "a"),
        ("run(a, key=b, *c, **d)\n", "a b c d"),
        ("def f(x=a, *, y: int = b): return c\n", "a b c"),
        ("x = lambda y=a: b\n", "a b"),
        ("@run(a)\ndef f(): pass\n", "a"),
        ("class C(run(a)): pass\n", "a"),
        ("try: pass\nexcept a as e: pass\n", "a"),
        ("try: pass\nexcept (a, b): raise c from d\n", "a b c d"),
        ("with a as x, b: pass\n", "a b"),
        ("for x in a: pass\n", "a"),
        ("x = [a for y in b if c]\n", "a b c"),
        ("x = {a: b, **c}\n", "a b c"),
        ("x = a[b:c]\n", "a b c"),
        ("a[b] = c\n", "a b c"),
        ("x, y = a, b\n", "a b"),
        ("x = (a)\n", "a"),
        ("if (x := a) and b: assert c, d\n", "a b c d"),
        ("x = f'{a:{b}}'\n", "a b"),
        ("match a:\n    case int(x) if b: pass\n", "a b"),
        ("match a:\n    case b(x): pass\n", "a b"),
        ("del a[b]\n", "a b"),
        (
            "async def f():\n    await a\n    yield b\n    return c\n",
            "a b c",
        ),
        ("def f(x: Annotated[int, run(a)]): pass\n", "a"),
        ("print(a if b else not c, -d)\n", "a b c d"),
    ] {
        assert_eq!(read(body), expected, "{body}");
    }
}

#[test]
fn test_python_names_that_load_no_value_are_not_read() {
    for body in [
        "a()\n",
        "@a\ndef f(): pass\n",
        "run(a=1)\n",
        "a = 1\n",
        "a: int = 1\n",
        "for a in (): pass\n",
        "with run() as a: pass\n",
        "x.a\n",
        "def a(b, *c, **d): pass\n",
        "lambda a, *b, **c: 0\n",
        "def f(*b: int, **c: int): pass\n",
        "class a: pass\n",
        "import a.b as c\n",
        "from a import b as c\n",
        "from .a import b\n",
        "def f():\n    nonlocal_ = 1\n    def g():\n        nonlocal nonlocal_\n",
        "def f(x: a) -> b: pass\n",
        "class C(a, metaclass=b): pass\n",
        "match 1:\n    case int(a=1): pass\n",
        "match 1:\n    case [a, *b] | {'k': c, **d}: pass\n",
        "match 1:\n    case int() as a: pass\n",
        "(a := 1)\n",
    ] {
        assert_eq!(read(body), "", "{body}");
    }
}

#[test]
fn test_python_names_of_locals_alone_are_left_out() {
    for source in [
        "def f(x):\n    y = x\n    return y\n",
        "x = lambda y: y\n",
        "print([y for y in (1,)])\n",
        "def f():\n    with run() as y: print(y)\n",
        "def f():\n    count = 0\n    count += 1\n    del count\n",
    ] {
        assert_eq!(captured(source), [], "{source}");
    }
}

#[test]
fn test_python_names_the_file_shares_are_read() {
    for (source, expected) in [
        ("def f():\n    import m\n    run(m)\n", "m"),
        ("def f():\n    from m import n\n    run(n)\n", "n"),
        ("def f():\n    def g(): pass\n    return g\n", "g"),
        ("def f():\n    class G: pass\n    return G\n", "G"),
        ("class C:\n    x = 1\n    y = x\n", "x"),
        ("def f():\n    global g\n    return g\n", "g"),
    ] {
        let names = captured(source);
        let names = names.iter().map(|(_, name, _)| *name).collect::<Vec<_>>();
        assert_eq!(names, [expected], "{source}");
    }
}

#[test]
fn test_python_names_of_the_module_and_classes_are_written() {
    use ReferenceRole::{Read, Write};
    for (source, expected) in [
        ("n += 1\n", vec![Read, Write]),
        ("del n\n", vec![Write]),
        (
            "class C:\n    n += 1\n    del n\n",
            vec![Read, Write, Write],
        ),
        ("def f():\n    global n\n    n = 1\n", vec![Write]),
        ("def f():\n    global n\n    n += 1\n", vec![Read, Write]),
        ("def f():\n    global n\n    del n\n", vec![Write]),
        (
            "def f():\n    global n\n    x, (n, *y) = 1, 2\n",
            vec![Write],
        ),
        (
            "def f():\n    global n\n    for n in (): pass\n",
            vec![Write],
        ),
        (
            "def f():\n    global n\n    with run() as n: pass\n",
            vec![Write],
        ),
        (
            "def f():\n    global n\n    [n := y for y in ()]\n",
            vec![Write],
        ),
        ("def f():\n    global n\n    [n for n in ()]\n", vec![Read]),
        (
            "def f():\n    global n\n    def g():\n        n = 1\n",
            vec![],
        ),
        (
            "def f():\n    n = 1\n    def g():\n        nonlocal n\n        n = 2\n",
            vec![],
        ),
    ] {
        let names = captured(source);
        assert!(names.iter().all(|(_, name, _)| *name == "n"), "{source}");
        let roles = names.iter().map(|(_, _, role)| *role).collect::<Vec<_>>();
        assert_eq!(roles, expected, "{source}");
    }
}
