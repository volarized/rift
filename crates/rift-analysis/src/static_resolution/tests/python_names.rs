//! Python names read and written as values.

use super::{ReferenceRole, StaticResolution, WORK_MAX, resolution, source_of, target};
use rift_protocol::read::EdgeKind;

/// The accesses of `main.py` that spell a plain name, as `from role name -> to`.
fn accesses(sources: &[(&str, &str)]) -> (Vec<String>, StaticResolution) {
    let result = resolution("python", sources, &[], WORK_MAX);
    assert!(!result.exhausted);
    let prefix = "rift://symbol/local/python/";
    let short = |identity: &str| identity.trim_start_matches(prefix).to_owned();
    let source = sources[0].1;
    let mut found = result
        .occurrences
        .iter()
        .filter(|access| {
            access.edge_kind.is_none()
                && matches!(access.role, ReferenceRole::Read | ReferenceRole::Write)
                && access.binding.unit().to_string() == "rift://source/project/main.py"
                && !source_of(access, source).contains('.')
        })
        .map(|access| {
            let from = access.edge_from.as_ref().map(|from| short(from.as_str()));
            let role = if access.role == ReferenceRole::Read {
                "reads"
            } else {
                "writes"
            };
            let line = source[..usize::try_from(access.binding.range().start()).expect("start")]
                .matches('\n')
                .count();
            let from = from.unwrap_or_default();
            (
                access.binding.range().start(),
                format!("{line} {from} {role} {}", short(target(access))),
            )
        })
        .collect::<Vec<_>>();
    found.sort();
    (found.into_iter().map(|(_, text)| text).collect(), result)
}

/// The accesses of one module and its count of reads left without a target.
fn module(body: &str) -> (Vec<String>, u64) {
    let (found, result) = accesses(&[("main.py", body)]);
    let refused = result.omitted.get(&EdgeKind::Reads).copied();
    (found, refused.unwrap_or_default())
}

const NAMES: &str = "def f(): pass\nclass C: pass\nN = 1\n";

/// The accesses of a body below [`NAMES`], without any refused; lines count from its first one.
fn below(body: &str) -> Vec<String> {
    let (found, refused) = module(&format!("{NAMES}{body}"));
    assert_eq!(refused, 0, "{body}");
    found
        .iter()
        .map(|access| {
            let (line, rest) = access.split_once(' ').expect("line");
            let line = line.parse::<usize>().expect("line") - NAMES.lines().count();
            format!("{line} {rest}").replace("main/", "")
        })
        .collect()
}

/// Asserts the reads of each body.
fn assert_reads(cases: &[(&str, &[&str])]) {
    for (body, expected) in cases {
        let mut found = below(body);
        found.retain(|access| access.contains(" reads "));
        assert_eq!(found, *expected, "{body}");
    }
}

#[test]
fn test_python_names_read_as_values_name_their_declaration() {
    assert_reads(&[
        ("g = f\n", &["0  reads f"]),
        (
            "def run(): return f, C, N\n",
            &["0 run reads f", "0 run reads C", "0 run reads N"],
        ),
        ("def run():\n    f(key=f)\n", &["1 run reads f"]),
        (
            "def run(x=N, *, y: int = N): pass\n",
            &["0 run reads N", "0 run reads N"],
        ),
        ("@f(N)\ndef run(): pass\n", &["0 run reads N"]),
        (
            "def run():\n    try: pass\n    except C: raise N\n",
            &["2 run reads C", "2 run reads N"],
        ),
        (
            "def run(x): return isinstance(x, C) and N[0]\n",
            &["0 run reads C", "0 run reads N"],
        ),
        (
            "def run():\n    def inner(): pass\n    return inner\n",
            &["2 run reads run/inner"],
        ),
        (
            "class D:\n    m = N\n    n = m\n",
            &["1  reads N", "2  reads D/m"],
        ),
        (
            "def run(): return [N for x in f for y in N]\n",
            &["0 run reads N", "0 run reads f", "0 run reads N"],
        ),
        ("x = lambda y=N: f\n", &["0  reads N", "0  reads f"]),
        (
            "def run():\n    global N\n    return N\n",
            &["2 run reads N"],
        ),
        (
            "def run():\n    global G\n    G = 1\ndef other(): return G\n",
            &["3 other reads G"],
        ),
        (
            "def run():\n    while N: f(N)\n",
            &["1 run reads N", "1 run reads N"],
        ),
    ]);
}

#[test]
fn test_python_callees_and_stated_names_are_no_reads() {
    assert_reads(&[
        ("f()\n", &[]),
        ("C()\n", &[]),
        ("N()\n", &[]),
        ("@f\ndef run(): pass\n", &[]),
        ("def run(N: C) -> C: pass\n", &[]),
        ("class D(C): pass\n", &[]),
        ("run(N=1)\n", &[]),
        ("x.N = x.f\n", &[]),
    ]);
}

#[test]
fn test_python_names_shadowed_in_their_scope_are_no_reads() {
    assert_reads(&[
        ("def run(f): return f\n", &[]),
        ("def run(*f, **N): return f, N\n", &[]),
        ("def run(*f: int): return f\n", &[]),
        ("def run():\n    f = 1\n    return f\n", &[]),
        ("def run():\n    print(f)\n    f = 1\n", &[]),
        ("def run():\n    for f in (): print(f)\n", &[]),
        ("def run():\n    with x as f: print(f)\n", &[]),
        ("def run():\n    with x as (f, [N]): print(f, N)\n", &[]),
        (
            "def run():\n    try: pass\n    except x as f: print(f)\n",
            &[],
        ),
        (
            "def run(x):\n    match x:\n        case [f, *N]: print(f, N)\n",
            &[],
        ),
        ("def run():\n    if (f := 1): print(f)\n", &[]),
        ("def run():\n    [f := x for x in ()]\n    print(f)\n", &[]),
        ("def run():\n    f += 1\n", &[]),
        ("def run():\n    print(f)\n    del f\n", &[]),
        ("def run(): return [f for f in ()]\n", &[]),
        ("def run(): return {f: N for f, N in ()}\n", &[]),
        ("def run(): return lambda f: f\n", &[]),
        ("def run(f):\n    def inner(): return f\n", &[]),
        ("def run(f):\n    class D:\n        x = f\n", &[]),
        (
            "def run():\n    f = 1\n    def inner():\n        nonlocal f\n        f = 2\n        return f\n",
            &[],
        ),
        ("def run(f=f): return f\n", &["0 run reads f"]),
        ("x = lambda f=f: f\n", &["0  reads f"]),
        ("def run(f):\n    def inner(x=f): pass\n", &[]),
        ("def run(): return [f for f in f]\n", &["0 run reads f"]),
        (
            "def run(): return [[f for f in f] for x in N]\n",
            &["0 run reads f", "0 run reads N"],
        ),
        ("def run[f](): return f\n", &[]),
    ]);
}

#[test]
fn test_python_class_names_are_read_in_the_class_body_alone() {
    assert_reads(&[
        (
            "class D:\n    f = 1\n    def m(self): return f\n",
            &["2 D/m reads f"],
        ),
        ("class D:\n    f = [f]\n", &["1  reads f"]),
        ("class D:\n    f = f\n", &["1  reads f"]),
        ("class D:\n    x = f\n    f = 1\n", &["1  reads f"]),
        ("class D:\n    m = 1\n    x = [m for y in ()]\n", &[]),
        (
            "class D:\n    m = ()\n    x = [y for y in m]\n",
            &["2  reads D/m"],
        ),
        (
            "class D:\n    m = 1\n    x = lambda self, y=m: y\n",
            &["2  reads D/m"],
        ),
        (
            "class D:\n    m = 1\n    def n(self, y=m): return m\n",
            &["2 D/n reads D/m"],
        ),
        ("class D:\n    m = 1\n    class E:\n        x = m\n", &[]),
        (
            "class D:\n    f = 1\n    class E:\n        x = f\n",
            &["3  reads f"],
        ),
        ("class D(x=C):\n    C = 1\n", &[]),
        (
            "class D:\n    for x in ():\n        y = z\n        z = 1\n",
            &["2  reads D/z"],
        ),
    ]);
}

#[test]
fn test_python_names_bound_more_than_once_need_one_declaration() {
    assert_reads(&[
        (
            "if x: M = 1\nelse: M = 2\ndef run(): return M\n",
            &["2 run reads M"],
        ),
        (
            "if x:\n    def g(): pass\nelse:\n    def g(): pass\ny = g\n",
            &["4  reads g"],
        ),
        (
            "def g(): pass\ng = f(g)\ny = g\n",
            &["1  reads g", "2  reads g"],
        ),
        (
            "def run():\n    def g(): pass\n    g = f(g)\n    return g\n",
            &["2 run reads run/g", "3 run reads run/g"],
        ),
        (
            "def run():\n    g = 1\n    if g:\n        def g(): pass\n    return g\n",
            &["4 run reads run/g"],
        ),
        (
            "M = None\ndef run():\n    global M\n    M = 1\n    return M\n",
            &["4 run reads M"],
        ),
        ("for M in ():\n    x = M\n", &[]),
        ("M = 1\nfor M in ():\n    x = M\n", &["2  reads M"]),
        (
            "def run():\n    if x:\n        def g(): pass\n    else:\n        def g(): pass\n    return g\n",
            &["5 run reads run/g"],
        ),
    ]);
}

/// Asserts that a body below [`NAMES`] stores no access and counts `refused` reads.
fn assert_refused(cases: &[(&str, u64)]) {
    for (body, refused) in cases {
        let (found, omitted) = module(&format!("{NAMES}{body}"));
        assert_eq!(found, Vec::<String>::new(), "{body}");
        assert_eq!(omitted, *refused, "{body}");
    }
}

#[test]
fn test_python_names_without_one_declaration_are_counted() {
    assert_refused(&[
        // The module has not bound the name yet.
        ("x = M\nM = 1\n", 1),
        ("M = [M]\n", 1),
        // A condition the class body states can leave the name of the module.
        ("class D:\n    if x: f = 1\n    y = f\n", 1),
        // The parameter and the declaration are two things.
        (
            "def run(g):\n    if not g:\n        def g(): pass\n    return g\n",
            2,
        ),
        // A star import can bind any name.
        ("from x import *\ny = f\n", 1),
    ]);
}

#[test]
fn test_python_locals_below_a_star_import_are_not_counted() {
    assert_refused(&[("from x import *\ndef run(f): return f\n", 0)]);
}

/// The reads of `main.py` beside the modules of a package, with the external ones last, and
/// the count of reads left without a target.
fn imported(body: &str) -> (Vec<String>, u64) {
    let (mut found, result) = accesses(&[
        ("main.py", body),
        (
            "pkg/__init__.py",
            "from .leaf import g as h\nfrom . import leaf\n",
        ),
        (
            "pkg/leaf.py",
            "def g(): pass\nclass K: pass\nV = 1\nif V: V = 2\ntry: from .other import both\nexcept ImportError: from .more import both\ntry: from .other import two\nexcept ImportError: from .more import two\n",
        ),
        (
            "pkg/other.py",
            "from .base import both\nfrom .base import one as two\n",
        ),
        ("pkg/more.py", "from .base import both, two\n"),
        ("pkg/base.py", "both = one = two = 1\n"),
        ("pkg/circle.py", "from .ring import z\n"),
        ("pkg/ring.py", "from .circle import z\n"),
    ]);
    found.extend(
        result
            .external
            .iter()
            .filter(|external| external.role == ReferenceRole::Read)
            .map(|external| {
                external
                    .target
                    .as_str()
                    .rsplit("/python/")
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            }),
    );
    let refused = result.omitted.get(&EdgeKind::Reads).copied();
    (found, refused.unwrap_or_default())
}

#[test]
fn test_python_imported_names_read_the_declaration_the_import_ends_at() {
    for (body, expected, refused) in [
        ("from pkg.leaf import g\nx = g\n", "1  reads pkg/leaf/g", 0),
        ("from pkg import h\nx = h\n", "1  reads pkg/leaf/g", 0),
        (
            "from pkg.leaf import K as L\ndef f(): return L\n",
            "1 main/f reads pkg/leaf/K",
            0,
        ),
        ("from pkg.leaf import V\nx = V\n", "1  reads pkg/leaf/V", 0),
        ("from pkg import leaf\nx = leaf\n", "1  reads pkg/leaf", 0),
        ("import pkg.leaf as m\nx = m\n", "1  reads pkg/leaf", 0),
        ("import pkg.leaf\nx = pkg\n", "1  reads pkg", 0),
        (
            "def f():\n    from pkg.leaf import g\n    return g\n",
            "2 main/f reads pkg/leaf/g",
            0,
        ),
        (
            "try: from pkg.leaf import g\nexcept ImportError: from pkg import h as g\nx = g\n",
            "2  reads pkg/leaf/g",
            0,
        ),
        (
            "from pkg.leaf import both\nx = both\n",
            "1  reads pkg/base/both",
            0,
        ),
        ("import os\nx = os\n", "os", 0),
        (
            "from os import path as p\ndef f(): return p\n",
            "os/path",
            0,
        ),
        ("from pkg.leaf import two\nx = two\n", "", 1),
        ("from pkg.circle import z\nx = z\n", "", 1),
        ("from pkg.leaf import missing\nx = missing\n", "", 1),
        (
            "try: from pkg.leaf import g\nexcept ImportError: g = None\nx = g\n",
            "",
            1,
        ),
        (
            "try: from pkg.leaf import g\nexcept ImportError: from pkg.leaf import K as g\nx = g\n",
            "",
            1,
        ),
        ("from pkg.leaf import g\nfor g in (): pass\nx = g\n", "", 1),
        (
            "import os\ndef f():\n    global os\n    os = None\ndef g(): return os\n",
            "",
            1,
        ),
        ("import os\ndef f(os): return os\n", "", 0),
        ("def f():\n    import os\ndef g(): return os\n", "", 0),
    ] {
        let (found, omitted) = imported(body);
        assert_eq!(found.join(" "), expected, "{body}");
        assert_eq!(omitted, refused, "{body}");
    }
}

/// Asserts the reads and writes of each body.
fn assert_accesses(cases: &[(&str, &[&str])]) {
    for (body, expected) in cases {
        assert_eq!(below(body), *expected, "{body}");
    }
}

#[test]
fn test_python_names_of_the_module_and_classes_are_written() {
    assert_accesses(&[
        ("N += 1\n", &["0  reads N", "0  writes N"]),
        ("del N\n", &["0  writes N"]),
        (
            "del f, (C, [N])\n",
            &["0  writes f", "0  writes C", "0  writes N"],
        ),
        ("def run():\n    global N\n    N = 2\n", &["2 run writes N"]),
        (
            "def run():\n    global N\n    N += 2\n",
            &["2 run reads N", "2 run writes N"],
        ),
        (
            "def run():\n    global f\n    f = N\n",
            &["2 run writes f", "2 run reads N"],
        ),
        ("def run():\n    global N\n    del N\n", &["2 run writes N"]),
        (
            "def run():\n    global N\n    for N in (): pass\n",
            &["2 run writes N"],
        ),
        (
            "def run():\n    global N\n    with x as N: pass\n",
            &["2 run writes N"],
        ),
        ("def run():\n    global G\n    G = 1\n", &["2 run writes G"]),
        (
            "def run():\n    def inner():\n        global N\n        N = 1\n",
            &["3 run/inner writes N"],
        ),
        (
            "class D:\n    m = 1\n    m += 1\n    del m\n",
            &["2  reads D/m", "2  writes D/m", "3  writes D/m"],
        ),
        ("class D:\n    N += 1\n", &["1  reads N"]),
        ("N = 2\n", &[]),
        ("def run():\n    N = 1\n    N += 1\n    del N\n", &[]),
        (
            "def run(N):\n    def inner():\n        nonlocal N\n        N += 1\n",
            &[],
        ),
        ("def run():\n    global H\n    with x as H: pass\n", &[]),
    ]);
}

#[test]
fn test_python_writes_without_one_declaration_are_counted() {
    for (body, reads, writes) in [
        ("import os\nos += 1\n", 0, 1),
        ("import os\ndef run():\n    global os\n    os = 1\n", 0, 1),
        ("from x import *\nN = 1\ndel N\n", 0, 1),
    ] {
        let (found, result) = accesses(&[("main.py", body)]);
        assert_eq!(found, Vec::<String>::new(), "{body}");
        let refused = |kind| result.omitted.get(&kind).copied().unwrap_or_default();
        assert_eq!(refused(EdgeKind::Reads), reads, "{body}");
        assert_eq!(refused(EdgeKind::Writes), writes, "{body}");
    }
}

#[test]
fn test_python_first_names_are_read_where_the_path_is_not_established_further() {
    assert_reads(&[
        ("x = N.real\n", &["0  reads N"]),
        ("x = N.real.imag\n", &["0  reads N"]),
        ("N.append(f.x)\n", &["0  reads N", "0  reads f"]),
        ("class D:\n    m = 1\nx = D.m\n", &[]),
        ("class D:\n    m = 1\nx = D.m.real\n", &[]),
        ("class D:\n    def m(self): pass\nD.m()\n", &[]),
        ("class D:\n    m = 1\nD.n = D.m\n", &["2  reads D"]),
        ("x = N[0].y\n", &["0  reads N"]),
        ("x = (N\n     .real)\n", &["0  reads N"]),
        ("def run(N): return N.real\n", &[]),
        ("def run(x: N.real): pass\n", &[]),
        ("class D(C.x): pass\n", &[]),
        ("match x:\n    case C.red: pass\n", &["1  reads C"]),
        (
            "class D:\n    red = 1\nmatch x:\n    case D.red: pass\n",
            &[],
        ),
        (
            "class D:\n    @property\n    def v(self): pass\n    @v.setter\n    def v(self, x): pass\n",
            &["3 D/v reads D/v"],
        ),
        ("del N.x\n", &["0  reads N"]),
        ("N.x += 1\n", &["0  reads N"]),
    ]);
}

#[test]
fn test_python_first_names_of_imported_modules_are_read_once() {
    for (body, expected, refused) in [
        ("import pkg.leaf as m\nx = m.g\n", "", 0),
        (
            "import pkg.leaf as m\nx = m.missing\n",
            "1  reads pkg/leaf",
            0,
        ),
        ("from pkg import leaf\nleaf.g()\n", "", 0),
        ("import os\nos.getcwd()\n", "", 0),
        ("import os\nx = os.environ\n", "os", 0),
        ("from pkg.leaf import two\nx = two.y\n", "", 1),
    ] {
        let (found, omitted) = imported(body);
        assert_eq!(found.join(" "), expected, "{body}");
        assert_eq!(omitted, refused, "{body}");
    }
}
