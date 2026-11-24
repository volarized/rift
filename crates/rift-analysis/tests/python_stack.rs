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
    let limits = ExactPackageLimits::new(2_048, 1 << 26).with_syntax(syntax);
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

/// Analyzes one module that holds `links`, and returns its edge count.
fn chained(text: String) -> usize {
    edges(&module(text), 65_536).len()
}

#[test]
fn a_chain_of_constructed_values_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}()"));
    assert!(chained(format!("{CLASS}a0 = C\n{links}a{LAST}.m()\n")) > 0);
}

#[test]
fn a_chain_of_constructed_attributes_ends() {
    let links = chain(|link, before| format!("        self.a{link} = self.a{before}()"));
    assert!(
        chained(format!(
            "{CLASS}class A:\n    def f(self):\n        self.a0 = C\n{links}        self.a{LAST}.m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_aliases_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}"));
    assert!(
        chained(format!(
            "{CLASS}a0 = C\n{links}a{LAST}().m()\nclass D(a{LAST}): pass\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_container_aliases_ends() {
    let links = chain(|link, before| format!("a{link} = a{before}"));
    assert!(
        chained(format!(
            "{CLASS}from typing import List\na0: List[C] = []\n{links}for x in a{LAST}:\n    x.m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_container_elements_ends() {
    let links = chain(|link, before| {
        format!("    xs{link}: \"List[x{before}]\" = []\n    for x{link} in xs{link}: pass")
    });
    assert!(
        chained(format!(
            "{CLASS}from typing import List\ndef f(xs0: List[C]):\n    for x0 in xs0: pass\n{links}    x{LAST}.m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_reexports_ends() {
    // Every name of the chain is an import of its own, so the work is quadratic.
    let links = LINKS / 4;
    let last = links - 1;
    let exports = chain_of(links, |link, before| {
        format!("from module import a{before} as a{link}")
    });
    let found = edges(
        &module(format!(
            "from outside import a0\n{exports}class K(a{last}): pass\n"
        )),
        512,
    );
    assert!(!found.is_empty());
}

#[test]
fn a_chain_of_bases_ends() {
    let links = chain(|link, before| format!("class C{link}(C{before}): pass"));
    assert!(
        chained(format!(
            "class C0:\n    def m(self): pass\n{links}C{LAST}().m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_returns_ends() {
    let links = chain(|link, before| {
        format!("class C{link}:\n    def n(self) -> \"C{before}\": return C{before}()")
    });
    let calls = ".n()".repeat(LAST);
    assert!(
        chained(format!(
            "{CLASS}class C0(C): pass\n{links}def f(x: C{LAST}):\n    x{calls}.m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_functions_ends() {
    let links = chain(|link, before| {
        format!("def f{link}() -> C:\n    return f{before}()\na{link} = f{before}()")
    });
    assert!(
        chained(format!(
            "{CLASS}def f0() -> C: ...\n{links}a{LAST}.m()\nf{LAST}().m()\n"
        )) > 0
    );
}

#[test]
fn a_chain_of_arguments_ends() {
    let links = chain(|link, before| {
        format!("def f{link}(x) -> C: ...\na{link} = f{before}(f{link}(a{before}))")
    });
    assert!(
        chained(format!(
            "{CLASS}def f0(x) -> C: ...\na0 = C()\n{links}a{LAST}.m()\n"
        )) > 0
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
fn a_chain_of_modules_ends() {
    let text = |link: usize| match link {
        0 => format!("{CLASS}a = C\n"),
        link => format!(
            "from chain.m{} import a as b\na = b\nc = b()\nc.m()\n",
            link - 1
        ),
    };
    let names = (0..LINKS / 4)
        .map(|link| format!("chain/m{link}"))
        .collect::<Vec<_>>();
    let mut modules = names
        .iter()
        .enumerate()
        .map(|(link, name)| (name.as_str(), text(link)))
        .collect::<Vec<_>>();
    modules.push(("chain/__init__", String::new()));
    assert!(!edges(&modules, 512).is_empty());
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
