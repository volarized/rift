//! Publication of Rust implementations written outside their type's module (#775).

use super::*;

const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";
const LIB: &str = "use std::fmt;\npub struct Version { pub major: u64 }\npub trait Parse { fn parse(text: &str) -> Self; }\npub fn leaf() {}\nmod display;\nmod parse;\nmod jobs;\n";
const DISPLAY: &str = "use crate::Version;\nuse core::fmt;\nimpl fmt::Display for Version { fn fmt(&self, _f: &mut fmt::Formatter) -> fmt::Result { self.helper(); Ok(()) } }\n";
const PARSE: &str = "use crate::{Parse, Version as Parsed};\nimpl Parse for Parsed { fn parse(_text: &str) -> Self { Parsed { major: 1 } } }\n";
const JOBS: &str = "use super::{leaf, Version};\nimpl Version { pub fn helper(&self) -> u64 { leaf(); self.major } }\n";

#[test]
fn implementations_are_facets_of_the_imported_type() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        vec![
            ("src/lib.rs", LIB),
            ("src/display.rs", DISPLAY),
            ("src/parse.rs", PARSE),
            ("src/jobs.rs", JOBS),
        ],
        None,
        Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
    )
    .expect("captured Rust package");
    let publication = analysis.publication();
    for identity in publication
        .objects
        .iter()
        .filter_map(|object| object.id.as_ref())
        .map(rift_protocol::read::SymbolId::as_str)
        .chain(
            publication
                .edges
                .iter()
                .flat_map(|edge| [edge.from.as_str(), edge.to.as_str()]),
        )
    {
        for moved in ["/display/Version", "/parse/Parsed", "/jobs/Version"] {
            assert!(
                !identity.contains(moved),
                "module-local identity {identity}"
            );
        }
    }
    let version = format!("{ROOT}/Version");
    let units = publication
        .declarations
        .iter()
        .filter(|declaration| declaration.symbol.as_str() == version)
        .map(|declaration| {
            declaration
                .unit
                .0
                .rsplit('/')
                .next()
                .expect("file")
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        units,
        ["display.rs", "jobs.rs", "lib.rs", "parse.rs"]
            .map(str::to_owned)
            .into(),
        "the type and its implementations are facets of one symbol"
    );
    let edge = |kind: EdgeKind, from: &str, to: &str| {
        publication.edges.iter().any(|edge| {
            edge.kinds.contains(&kind)
                && edge.from.as_str() == format!("{ROOT}/{from}")
                && edge.to.as_str() == format!("{ROOT}/{to}")
        })
    };
    assert!(
        publication.edges.iter().all(|edge| {
            !edge.kinds.contains(&EdgeKind::Contains)
                || edge.to.as_str() != version
                || edge.from.as_str() == ROOT
        }),
        "a module that holds an implementation does not contain its type"
    );
    assert!(
        publication.edges.iter().any(|edge| {
            edge.kinds.contains(&EdgeKind::Imports)
                && edge.from.as_str() == format!("{ROOT}/display")
                && edge.to.as_str().ends_with("/core/fmt")
        }),
        "an import belongs to the module of its file"
    );
    for (kind, from, to) in [
        (EdgeKind::Contains, "Version", "Version/major"),
        (EdgeKind::Contains, "Version", "Version/helper"),
        (EdgeKind::Contains, "Version", "Version/fmt"),
        (EdgeKind::Contains, "Version", "Version/parse"),
        (EdgeKind::Implements, "Version", "Parse"),
        (EdgeKind::Overrides, "Version/parse", "Parse/parse"),
        (EdgeKind::Calls, "Version/helper", "leaf"),
        (EdgeKind::Calls, "Version/fmt", "Version/helper"),
        (EdgeKind::Reads, "Version/helper", "Version/major"),
    ] {
        assert!(edge(kind, from, to), "{kind:?} {from} -> {to}");
    }
}

/// A module that reexports a type and implements it publishes the alias beside the type's
/// facets, and every declaration once.
#[test]
fn a_reexporting_module_that_implements_the_type_publishes_both_symbols() {
    for facade in [
        "pub use crate::task::Task;\nimpl Task { pub fn start(&self) -> u64 { self.major } }\n",
        "impl Task { pub fn start(&self) -> u64 { self.major } }\npub use crate::task::Task;\n",
    ] {
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::Rust,
            vec![
                ("src/lib.rs", "pub mod facade;\npub mod task;\n"),
                ("src/task.rs", "pub struct Task { pub major: u64 }\n"),
                ("src/facade.rs", facade),
            ],
            None,
            Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
        )
        .unwrap_or_else(|error| panic!("{facade}: {error:?}"));
        let publication = analysis.publication();
        let declared = publication
            .declarations
            .iter()
            .map(|declaration| {
                let unit = declaration.unit.0.rsplit('/').next().expect("file");
                let symbol = declaration.symbol.as_str();
                (
                    symbol.strip_prefix(ROOT).unwrap_or(symbol).to_owned(),
                    unit.to_owned(),
                )
            })
            .collect::<Vec<_>>();
        for (symbol, unit) in [
            ("/task/Task", "task.rs"),
            ("/task/Task", "facade.rs"),
            ("/task/Task/start", "facade.rs"),
            ("/facade/Task", "facade.rs"),
        ] {
            assert_eq!(
                declared
                    .iter()
                    .filter(|(found, file)| found == symbol && file == unit)
                    .count(),
                1,
                "{facade}: {symbol} in {unit}: {declared:?}"
            );
        }
        for (kind, from, to) in [
            (EdgeKind::Aliases, "facade/Task", "task/Task"),
            (EdgeKind::Contains, "task/Task", "task/Task/start"),
            (EdgeKind::Reads, "task/Task/start", "task/Task/major"),
        ] {
            assert!(
                publication.edges.iter().any(|edge| {
                    edge.kinds.contains(&kind)
                        && edge.from.as_str() == format!("{ROOT}/{from}")
                        && edge.to.as_str() == format!("{ROOT}/{to}")
                }),
                "{facade}: {kind:?} {from} -> {to}"
            );
        }
    }
}

/// Where a symbol's definition is read from: its published object and its document.
struct Defined<'case> {
    symbol: &'case str,
    kind: &'case str,
    name: &'case str,
    visibility: &'case str,
    document: &'case str,
}

/// The fields of a symbol come from the declaration that defines it, whichever file the
/// provider reads first (#1025).
#[test]
fn a_symbol_reads_as_its_definition_in_every_file_order() {
    let task = |source, kind| (source, kind, "pub", "task.rs");
    for ((definition, kind, visibility, document), implementation) in [
        (
            task("/// The type.\npub struct Task;\n", "struct"),
            "pub use crate::task::Task;\n/// The block.\nimpl Task { pub fn start() {} }\n",
        ),
        (
            task("/// The type.\npub enum Task { One }\n", "enum"),
            "use crate::task::Task;\n/// The block.\nimpl Clone for Task { fn clone(&self) -> Self { Task::One } }\n",
        ),
        (
            task("/// The type.\npub struct Task<T>(pub T);\n", "struct"),
            "use crate::task::Task;\n/// The block.\nimpl<T: Copy> Task<T> { pub fn start(&self) -> T { self.0 } }\n",
        ),
        (
            task(
                "/// The type.\npub union Task { pub one: u8 }\n",
                "union_item",
            ),
            "/// The block.\nimpl crate::task::Task { pub fn start() {} }\n",
        ),
        (
            task("/// The type.\npub struct Task;\n", "struct"),
            "pub mod inner { use crate::task::Task; /// The block.\nimpl Task { pub fn start() {} } }\n",
        ),
        (
            task(
                "/// The type.\n#[cfg(unix)]\npub struct Task;\n/// The type.\n#[cfg(not(unix))]\npub struct Task;\n",
                "struct",
            ),
            "use crate::task::Task;\n/// The block.\nimpl Task { pub fn start() {} }\n",
        ),
        (
            task(
                "impl Task { pub fn stop() {} }\n/// The type.\npub struct Task;\n",
                "struct",
            ),
            "use crate::task::Task;\nimpl Task { pub fn start() {} }\nimpl Drop for Task { fn drop(&mut self) {} }\n",
        ),
        (
            (
                "/// The type.\npub(crate) struct Task;\n",
                "struct",
                "pub(crate)",
                "",
            ),
            "use crate::task::Task;\n/// The block.\nimpl Task { pub fn start() {} }\n",
        ),
    ] {
        for module in ["facade", "zfacade"] {
            let defined = Defined {
                symbol: "/task/Task",
                kind,
                name: "Task",
                visibility,
                document,
            };
            assert_defined(&[("task", definition), (module, implementation)], &defined);
        }
    }
}

/// A field and a method of one path share a symbol, and the field reads as it does when the
/// method is written in the type's own file (#1006).
#[test]
fn a_field_and_a_method_of_one_path_read_as_the_field_in_every_file_order() {
    for module in ["facade", "zfacade"] {
        assert_defined(
            &[
                ("task", "pub struct Task { pub size: u8 }\n"),
                (
                    module,
                    "use crate::task::Task;\nimpl Task { pub(crate) fn size(&self) -> u8 { 1 } }\n",
                ),
            ],
            &Defined {
                symbol: "/task/Task/size",
                kind: "field_declaration",
                name: "size",
                visibility: "pub",
                document: "task.rs",
            },
        );
    }
}

/// A member several implementations declare reads from the file that defines the type.
#[test]
fn a_member_of_several_implementations_reads_from_the_file_of_its_type() {
    for module in ["facade", "zfacade"] {
        assert_defined(
            &[
                (
                    "task",
                    "pub struct Task;\nimpl Task { /// The type.\npub(crate) fn size(&self) -> u8 { 1 } }\n",
                ),
                (
                    module,
                    "use crate::task::Task;\nimpl Task { /// The block.\npub fn size(&self) -> u16 { 1 } }\n",
                ),
            ],
            &Defined {
                symbol: "/task/Task/size",
                kind: "function",
                name: "size",
                visibility: "pub(crate)",
                document: &format!("{module}.rs"),
            },
        );
    }
}

fn assert_defined(modules: &[(&str, &str)], defined: &Defined<'_>) {
    let root = modules
        .iter()
        .flat_map(|(module, _)| ["pub mod ", module, ";\n"])
        .collect::<String>();
    let paths = modules
        .iter()
        .map(|(module, _)| format!("src/{module}.rs"))
        .collect::<Vec<_>>();
    let files = std::iter::once(("src/lib.rs", root.as_str()))
        .chain(
            paths
                .iter()
                .zip(modules)
                .map(|(path, (_, text))| (path.as_str(), *text)),
        )
        .collect::<Vec<_>>();
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        files,
        None,
        Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
    )
    .unwrap_or_else(|error| panic!("{modules:?}: {error:?}"));
    let publication = analysis.publication();
    let id = format!("{ROOT}{}", defined.symbol);
    let container = defined
        .symbol
        .rsplit_once('/')
        .filter(|(container, _)| container.ends_with("/Task"))
        .map(|(container, _)| format!("{ROOT}{container}"));
    let object = publication
        .objects
        .iter()
        .find(|object| object.id.as_ref().is_some_and(|held| held.as_str() == id))
        .unwrap_or_else(|| panic!("{modules:?}: {id}"));
    assert_eq!(
        (
            object.kind.0.as_str(),
            object.name.as_str(),
            object.visibility.as_deref(),
            object.container.as_ref().map(SymbolId::as_str),
        ),
        (
            defined.kind,
            defined.name,
            Some(defined.visibility),
            container.as_deref(),
        ),
        "{modules:?}"
    );
    if let Some(documentation) = object.documentation.first() {
        assert_eq!(documentation.text, "The type.", "{modules:?}");
    }
    let document = publication
        .documents
        .iter()
        .find(|document| document.identity == id)
        .map_or("", |document| {
            document.unit.0.rsplit('/').next().expect("file")
        });
    assert_eq!(document, defined.document, "{modules:?}");
}

#[test]
fn supported_rust_forms_keep_their_neighboring_package_calls() {
    const PREFIX: &str = "pub struct U; impl U { pub fn k(&self) -> Result<u32, u32> { Ok(1) } } pub struct Held { a: u8, b: u8 } pub fn leaf() {}";
    const AFTER: &str = "pub fn after(u: U) -> Result<u32,u32> { leaf(); u.k() }";
    for (body, leaf_calls, foreign) in [
        (
            "pub fn run(u: U) -> Result<u32,u32> { leaf(); try!(u.k()); u.k() }",
            1,
            false,
        ),
        (
            "pub fn run(u: U, held: Held) { leaf(); let Held { a, #[cfg(all())] b } = held; let _ = u.k(); }",
            1,
            false,
        ),
        (
            "pub fn run(u: U) { leaf(); { let Held { a, #[cfg(all())] b } = Held { a: 1, b: 2 }; leaf(); } let _ = u.k(); }",
            2,
            false,
        ),
        (
            "unsafe extern \"C\" { pub type Opaque; fn f(); } pub fn run(u: U) { leaf(); unsafe { f(); } let _ = u.k(); }",
            1,
            true,
        ),
    ] {
        let source = format!("{PREFIX}\n{body}\n{AFTER}");
        let analysis = fixture::package_result_with_metadata(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", &source)],
            None,
            Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
        )
        .expect("Rust package");
        let publication = analysis.publication();
        let count = |caller: &str, callee: &str| {
            publication
                .edges
                .iter()
                .filter(|edge| {
                    edge.kinds.contains(&EdgeKind::Calls)
                        && edge.from.as_str().ends_with(caller)
                        && edge.to.as_str().ends_with(callee)
                })
                .count()
        };
        assert_eq!(count("/run", "/leaf"), leaf_calls, "{body}");
        assert_eq!(count("/run", "/U/k"), 1, "{body}");
        assert_eq!(count("/after", "/leaf"), 1, "{body}");
        assert_eq!(count("/after", "/U/k"), 1, "{body}");
        assert_eq!(count("/run", "/f"), usize::from(foreign), "{body}");
    }
}

#[test]
fn a_foreign_type_defines_its_cross_module_implementation_identity() {
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        vec![
            ("src/lib.rs", "pub mod task; pub mod facade;\n"),
            (
                "src/task.rs",
                "unsafe extern \"C\" {\n    /// The type.\n    pub type Task;\n    pub static LIMIT: u32;\n    pub fn external();\n}\n",
            ),
            (
                "src/facade.rs",
                "use crate::task::Task;\n/// The implementation.\nimpl Task { pub fn start() {} }\n",
            ),
        ],
        None,
        Some(("Cargo.toml", "[package]\nname='beacon'\nversion='1.0.0'\n")),
    )
    .expect("Rust package with a foreign type and an implementation");
    let publication = analysis.publication();
    for (name, expected_kind) in [
        ("task/Task", "extern_type"),
        ("task/LIMIT", "static"),
        ("task/external", "function"),
        ("task/Task/start", "function"),
    ] {
        let id = format!("{ROOT}/{name}");
        let object = publication
            .objects
            .iter()
            .find(|item| item.id.as_ref().is_some_and(|symbol| symbol.as_str() == id))
            .unwrap_or_else(|| panic!("missing published object {name}"));
        assert_eq!(object.kind.0, expected_kind, "{name}");
    }
    let type_id = format!("{ROOT}/task/Task");
    let owners = publication
        .declarations
        .iter()
        .filter(|item| item.symbol.as_str() == type_id)
        .map(|item| {
            (
                item.unit.0.rsplit('/').next(),
                item.syntax_element_kind.as_deref(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        owners,
        [
            (Some("task.rs"), Some("extern_type_item")),
            (Some("facade.rs"), Some("impl_item")),
        ]
        .into()
    );
    assert!(
        publication.documents.iter().any(|document| {
            document.identity == type_id && document.unit.0.ends_with("/task.rs")
        })
    );
}
