//! Python package analysis fits a 2 MiB thread stack whatever its names refer to.
#![cfg(feature = "collector")]

use std::{collections::BTreeSet, fmt::Write as _};

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::read::PackageIdentity;
use rift_syntax::{ShippedLanguage, SyntaxLimits};

const STACK_BYTES: usize = 2 << 20;
const LINKS: usize = 1_000;
const CLASS: &str = "class C:\n    def m(self): pass\n";

/// The edges of one package analyzed on a 2 MiB stack, as `from -> to`.
fn edges(modules: &[(&str, String)], syntax_depth_max: usize) -> Vec<String> {
    let modules = modules
        .iter()
        .map(|(name, text)| ((*name).to_owned(), text.clone()))
        .collect::<Vec<_>>();
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || analyzed(&modules, syntax_depth_max))
        .expect("analysis thread")
        .join()
        .expect("analysis ends on its stack")
}

fn analyzed(modules: &[(String, String)], syntax_depth_max: usize) -> Vec<String> {
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("fixture owner");
    let language = ShippedLanguage::Python.language();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("fixture origin");
    let files = modules
        .iter()
        .map(|(name, text)| {
            let path = ProjectPath::new(format!("{name}.py")).expect("fixture path");
            (path, text.as_str())
        })
        .collect::<Vec<_>>();
    let sources = files
        .iter()
        .map(|(path, text)| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let names = modules
        .iter()
        .filter_map(|(name, _)| name.split('/').next())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let roots = [
        PackageImportRoot::new(None, names, PackageImportRootOrigin::Wheel).expect("fixture root"),
    ];
    let syntax = SyntaxLimits::new(1 << 24, 1 << 22, syntax_depth_max).expect("syntax limits");
    let limits = ExactPackageLimits::new(8_192, 1 << 26).with_syntax(syntax);
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("fixture input")
        .with_import_roots(&roots)
        .expect("fixture import root");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("package analysis");
    let mut edges = analysis
        .publication()
        .edges
        .iter()
        .map(|edge| {
            let name = |identity: &str| {
                identity
                    .rsplit_once("/python/")
                    .map_or(identity, |(_, name)| name)
                    .to_owned()
            };
            format!("{} -> {}", name(&edge.from.0), name(&edge.to.0))
        })
        .collect::<Vec<_>>();
    edges.sort();
    edges
}

fn module(text: impl Into<String>) -> [(&'static str, String); 1] {
    [("module", text.into())]
}

/// One line for each link of a chain, from the number of the link and the one before it.
fn chain(line: impl Fn(usize, usize) -> String) -> String {
    chain_of(LINKS, line)
}

fn chain_of(links: usize, line: impl Fn(usize, usize) -> String) -> String {
    (1..links).fold(String::new(), |mut text, link| {
        writeln!(text, "{}", line(link, link - 1)).expect("text line");
        text
    })
}

// Issue #1048.
#[test]
fn values_that_construct_each_other_end() {
    let attributes = "class A:\n    def __init__(self):\n        self.a = self.b()\n        \
        self.b = self.a()\n    def f(self):\n        self.a.f()\n        self.b.f()\n";
    let modules = [
        ("module", "from other import b\na = b()\na.m()\n".to_owned()),
        ("other", "from module import a\nb = a()\nb.m()\n".to_owned()),
    ];
    for depth in [512, 65_536] {
        let found = edges(&module(attributes), depth);
        assert!(
            found.contains(&"module/A/f -> module/A/a".to_owned()),
            "{found:?}"
        );
        let found = edges(&modules, depth);
        assert!(found.contains(&"module -> other/b".to_owned()), "{found:?}");
    }
}

/// The last link of a chain.
const LAST: usize = LINKS - 1;

/// The edges of one module at the deepest supported syntax.
fn chained(text: String) -> Vec<String> {
    edges(&module(text), 65_536)
}

/// Every edge of `expected` is in `found`, which holds `count` edges.
fn assert_edges(found: &[String], count: usize, expected: &[&str]) {
    for edge in expected {
        assert!(
            found.iter().any(|found| found == edge),
            "{edge} of {}",
            found.len()
        );
    }
    assert_eq!(found.len(), count);
}

#[test]
fn a_chain_of_constructed_values_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}()"));
    let found = chained(format!("{CLASS}a0 = C\n{links}a{LAST}.m()\n"));
    assert_edges(
        &found,
        1004,
        &["module/C -> module/C/m", "module -> module/C"],
    );
}

#[test]
fn a_chain_of_constructed_attributes_ends() {
    let links = chain(|link, before| format!("        self.a{link} = self.a{before}()"));
    let found = chained(format!(
        "{CLASS}class A:\n    def f(self):\n        self.a0 = C\n{links}        self.a{LAST}.m()\n"
    ));
    assert_edges(
        &found,
        3006,
        &["module/A -> module/A/f", "module/A/f -> module/A/a998"],
    );
}

#[test]
fn a_chain_of_aliases_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}"));
    let found = chained(format!(
        "{CLASS}a0 = C\n{links}a{LAST}().m()\nclass D(a{LAST}): pass\n"
    ));
    assert_edges(
        &found,
        2007,
        &[
            "module -> module/C/m",
            "module/D -> module/C",
            "module/D -> module/a999",
        ],
    );
}

#[test]
fn a_chain_of_container_aliases_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}"));
    let found = chained(format!(
        "{CLASS}from typing import List\na0: List[C] = []\n{links}for x in a{LAST}:\n    x.m()\n"
    ));
    assert_edges(&found, 2005, &["module -> module/a998"]);
}

#[test]
fn a_chain_of_container_elements_ends() {
    let links = chain(|link, before| {
        format!("    xs{link}: \"List[x{before}]\" = []\n    for x{link} in xs{link}: pass")
    });
    let found = chained(format!(
        "{CLASS}from typing import List\ndef f(xs0: List[C]):\n    for x0 in xs0: pass\n{links}    x{LAST}.m()\n"
    ));
    assert_edges(&found, 1005, &["module/C -> module/C/m"]);
}

#[test]
fn a_chain_of_reexports_ends() {
    let exports = chain(|link, before| format!("from module import a{before} as a{link}"));
    let found = edges(
        &module(format!(
            "{CLASS}from module import C as a0\n{exports}class K(a{LAST}): pass\nK().m()\n"
        )),
        512,
    );
    assert_edges(
        &found,
        1007,
        &["module/K -> module/C", "module -> module/C/m"],
    );
}

// Issue #1066.
#[test]
fn a_chain_of_module_imports_names_its_first_module() {
    const MODULES: usize = 2_000;
    let names = (0..MODULES)
        .map(|link| format!("chain/m{link}"))
        .collect::<Vec<_>>();
    let mut modules = names
        .iter()
        .enumerate()
        .map(|(link, name)| match link {
            0 => (name.as_str(), format!("{CLASS}a = C\n")),
            link => (
                name.as_str(),
                format!("from chain.m{} import a\n", link - 1),
            ),
        })
        .collect::<Vec<_>>();
    modules.push(("chain/__init__", String::new()));
    let last = MODULES - 1;
    modules.push((
        "chain/use",
        format!("from chain.m{last} import a\na().m()\n"),
    ));
    let found = edges(&modules, 512);
    let called = "chain/use -> chain/m0/C/m".to_owned();
    assert!(found.contains(&called), "{} edges", found.len());
    let imported = format!("chain/m{last} -> chain/m0/a");
    assert!(found.contains(&imported), "{} edges", found.len());
}

#[test]
fn module_imports_that_end_in_a_cycle_name_nothing() {
    let found = edges(
        &[
            ("chain/__init__", String::new()),
            ("chain/a", "from chain.b import x, y\nx.m()\n".to_owned()),
            (
                "chain/b",
                "from chain.c import x\nfrom chain.c import z as y\n".to_owned(),
            ),
            ("chain/c", format!("from chain.b import x\n{CLASS}z = C\n")),
            (
                "chain/d",
                "from chain.a import x, y\nx.m()\ny().m()\n".to_owned(),
            ),
        ],
        512,
    );
    assert!(
        found.contains(&"chain/d -> chain/c/C/m".to_owned()),
        "{found:?}"
    );
    assert!(!found.iter().any(|edge| edge.ends_with("/x")), "{found:?}");
}

#[test]
fn a_class_with_many_bases_inherits_from_each() {
    const BASES: usize = 4_000;
    let bases = (0..BASES).fold(String::new(), |mut text, base| {
        writeln!(text, "class B{base}:\n    def m{base}(self): pass").expect("text line");
        text
    });
    let names = (0..BASES)
        .map(|base| format!("B{base}"))
        .collect::<Vec<_>>()
        .join(", ");
    let last = BASES - 1;
    let found = edges(
        &module(format!(
            "{bases}class K({names}): pass\nK().m0()\nK().m{last}()\n"
        )),
        512,
    );
    assert_edges(
        &found,
        16_005,
        &[
            "module -> module/B0/m0",
            "module -> module/B3999/m3999",
            "module/K -> module/B0",
            "module/K -> module/B3999",
        ],
    );
}

#[test]
fn many_bases_that_share_a_base_merge_in_the_c3_order() {
    const BASES: usize = 1_000;
    let bases = (0..BASES).fold(String::new(), |mut text, base| {
        writeln!(text, "class B{base}(Root): pass").expect("text line");
        text
    });
    let names = (0..BASES)
        .map(|base| format!("B{base}"))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "class Root:\n    def m(self): pass\n    def r(self): pass\n{bases}class Last(Root):\n    \
        def m(self): pass\nclass K({names}, Last): pass\nK().m()\nK().r()\n\
        class Wrong(Root, B0): pass\nWrong().r()\n"
    );
    let found = edges(&module(text), 512);
    let called = |name: &str| found.contains(&format!("module -> {name}"));
    assert!(
        called("module/Last/m"),
        "`Last` precedes the base all share"
    );
    assert!(!called("module/Root/m"));
    assert!(called("module/Root/r"));
    let calls = found
        .iter()
        .filter(|edge| *edge == "module -> module/Root/r")
        .count();
    assert_eq!(calls, 1, "the call of `K().r`, none of `Wrong().r`");
}

// Issue #1065.
#[test]
fn a_long_base_class_chain_keeps_the_edges_of_its_file() {
    const CLASSES: usize = 10_000;
    let sentinel = "class Zed:\n    def m(self): pass\ndef sentinel(z: Zed):\n    z.m()\n";
    let last = CLASSES - 1;
    let links = chain_of(CLASSES, |link, before| {
        format!("class C{link}(C{before}): pass")
    });
    let found = chained(format!(
        "{sentinel}class C0:\n    def m(self): pass\n{links}C{last}().m()\n"
    ));
    assert_edges(
        &found,
        30_006,
        &[
            "module/sentinel -> module/Zed/m",
            "module/C9999 -> module/C9998",
            "module -> module/C0/m",
        ],
    );
    let links =
        chain(|link, before| format!("class C{link}(C{before}):\n    def m(self): super().m()"));
    let found = chained(format!(
        "{sentinel}class C0:\n    def m(self): pass\n{links}C{LAST}().m()\n"
    ));
    assert_edges(
        &found,
        7 * LINKS + 2,
        &[
            "module/sentinel -> module/Zed/m",
            "module/C999/m -> module/C998/m",
        ],
    );
}

#[test]
fn a_chain_of_bases_ends() {
    let links = chain(|link, before| format!("class C{link}(C{before}): pass"));
    let found = chained(format!(
        "class C0:\n    def m(self): pass\n{links}C{LAST}().m()\n"
    ));
    assert_edges(
        &found,
        3001,
        &["module -> module/C0/m", "module/C999 -> module/C998"],
    );
}

#[test]
fn a_chain_of_returns_ends() {
    let links = chain(|link, before| {
        format!("class C{link}:\n    def n(self) -> \"C{before}\": return C{before}()")
    });
    let calls = ".n()".repeat(LAST);
    let found = chained(format!(
        "{CLASS}class C0(C): pass\n{links}def f(x: C{LAST}):\n    x{calls}.m()\n"
    ));
    for link in LAST - 9..=LAST {
        let edge = format!("module/f -> module/C{link}/n");
        assert!(found.contains(&edge), "{edge} of {}", found.len());
    }
}

#[test]
fn a_chain_of_functions_ends() {
    let links = chain(|link, before| {
        format!("def f{link}() -> C:\n    return f{before}()\na{link} = f{before}()")
    });
    let found = chained(format!(
        "{CLASS}def f0() -> C: ...\n{links}a{LAST}.m()\nf{LAST}().m()\n"
    ));
    assert_edges(
        &found,
        5002,
        &["module -> module/C/m", "module/f999 -> module/f998"],
    );
}

#[test]
fn a_chain_of_arguments_ends() {
    let links = chain(|link, before| {
        format!("def f{link}(x) -> C: ...\na{link} = f{before}(f{link}(a{before}))")
    });
    let found = chained(format!(
        "{CLASS}def f0(x) -> C: ...\na0 = C()\n{links}a{LAST}.m()\n"
    ));
    assert_edges(
        &found,
        6001,
        &["module -> module/C/m", "module -> module/f999"],
    );
}

#[test]
fn a_chain_of_receivers_keeps_the_edges_of_its_file() {
    let calls = ".n()".repeat(LAST);
    let text = format!(
        "class C:\n    def n(self) -> \"C\": return self\n    def m(self): pass\ndef f(x: C):\n    \
        x{calls}.m()\ndef g(x: C):\n    x.n().n().m()\n"
    );
    let found = edges(&module(text), 65_536);
    let called = "module/g -> module/C/m".to_owned();
    assert!(found.contains(&called), "{} edges", found.len());
}

#[test]
fn a_chain_of_module_aliases_names_the_last_one() {
    // Every module follows the aliases before it, so the work is quadratic.
    const MODULES: usize = LINKS / 4;
    let text = |link: usize| match link {
        0 => format!("{CLASS}a = C\n"),
        link => format!(
            "from chain.m{} import a as b\na = b\nc = b()\nc.m()\n",
            link - 1
        ),
    };
    let names = (0..MODULES)
        .map(|link| format!("chain/m{link}"))
        .collect::<Vec<_>>();
    let mut modules = names
        .iter()
        .enumerate()
        .map(|(link, name)| (name.as_str(), text(link)))
        .collect::<Vec<_>>();
    modules.push(("chain/__init__", String::new()));
    let found = edges(&modules, 512);
    for edge in ["chain/m1 -> chain/m0/C/m", "chain/m249 -> chain/m248/a"] {
        assert!(
            found.contains(&edge.to_owned()),
            "{edge} of {}",
            found.len()
        );
    }
}

#[test]
fn nested_optional_annotations_name_their_class() {
    let nested = |levels: usize, inner: &str| {
        format!(
            "{}{inner}{}",
            "Optional[".repeat(levels),
            "]".repeat(levels)
        )
    };
    let called = "module/f -> module/C/m".to_owned();
    for levels in [1, 64, LINKS, 4 * LINKS] {
        let annotation = nested(levels, "C");
        let text =
            format!("from typing import Optional\n{CLASS}def f(x: \"{annotation}\"):\n    x.m()\n");
        let found = edges(&module(text), 512);
        assert!(found.contains(&called), "{levels}: {found:?}");
        let annotation = nested(levels, "List[C]");
        let text = format!(
            "from typing import List, Optional\n{CLASS}def f(x: \"{annotation}\"):\n    \
            for y in x:\n        y.m()\n"
        );
        let found = edges(&module(text), 512);
        assert!(found.contains(&called), "{levels}: {found:?}");
    }
}

// Issue #1054.
#[test]
fn one_reexported_name_of_a_statement_is_one_record() {
    let member = "def a(): pass\ndef c(): pass\n";
    for (statement, aliases) in [
        ("from .m import a as a, a as a\n", 1),
        ("from .m import a as a, c as c, a as a\n", 2),
        ("from .m import a as a\nfrom .m import a as a\n", 2),
        ("from .m import a, a\n", 0),
    ] {
        let found = edges(
            &[
                ("chain/__init__", statement.to_owned()),
                ("chain/m", member.to_owned()),
            ],
            512,
        );
        let count = found
            .iter()
            .filter(|edge| {
                ["chain/a -> chain/m/a", "chain/c -> chain/m/c"].contains(&edge.as_str())
            })
            .count();
        assert_eq!(count, aliases, "{statement}: {found:?}");
    }
}
