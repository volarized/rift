//! Publication of imports from an accepted Cargo library root.

use super::*;
use rift_protocol::read::{EdgeDerivation, EdgeProvider};

const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";
const DEBUG: &str = "rift://symbol/stdlib/rust/rust/std/fmt/Debug";
const VEC_NEW: &str = "rift://symbol/stdlib/rust/rust/alloc/vec/Vec/new";
const MANIFEST: &str = "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n";
const SOURCE: &str = "// é\nuse std::fmt::Debug;\nuse untracked::Thing;\n\
    pub mod sibling;\nuse crate::sibling::Local;\n\
    pub use crate::sibling::AliasSource as PublicType;\n\
    pub fn run() -> Vec<u8> { Vec::new() }\n";
const SIBLING: &str = "use std::fmt::Write;\npub struct Local;\npub struct AliasSource;\n";

fn analyze_rust_root(files: Vec<(&str, &str)>, manifest: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        files,
        None,
        Some(("Cargo.toml", manifest)),
    )
    .expect("captured Rust package")
}

const SELF_CRATE_ALIAS_SOURCE: &str = r"
pub extern crate self as facade;
pub fn leaf() -> u8 { 7 }
pub fn call_root() -> u8 { facade::leaf() }

pub mod inherited {
    pub fn call_child() -> u8 { facade::leaf() }
}

pub mod shadowed {
    mod facade {
        pub fn leaf() -> u16 { 120 }
    }
    pub fn call_shadow() -> u16 { facade::leaf() }
}

pub mod nested_alias {
    extern crate self as local_facade;
    pub fn call_nested() -> u8 { local_facade::leaf() }
    pub fn leaf() -> u32 { 92 }
    pub fn call_relative() -> u32 { self::leaf() }
}

pub mod inactive_alias {
    #[cfg(any())]
    extern crate self as disabled;
    mod disabled {
        pub fn leaf() -> i16 { -3 }
    }
    pub fn call_active() -> i16 { disabled::leaf() }
}

pub mod imports {
    pub mod facade {
        pub fn leaf() -> u64 { 12 }
    }
}
pub mod glob_shadowed {
    use crate::imports::*;
    pub fn call_glob() -> u64 { facade::leaf() }
}

pub mod thread {
    pub fn yield_now() {}
}
pub mod external_shadowed {
    extern crate std as facade;
    pub fn call_external() { facade::thread::yield_now(); }
}
";

#[test]
fn package_resolves_rust_self_crate_aliases_with_written_module_precedence() {
    use rift_protocol::read::EdgeKind;

    let source = SELF_CRATE_ALIAS_SOURCE;
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    let [unit] = publication.units.as_slice() else {
        panic!("one authored library unit");
    };
    for (owner, target, invocation) in [
        ("call_root", "leaf", "facade::leaf()"),
        ("inherited/call_child", "leaf", "facade::leaf()"),
        (
            "shadowed/call_shadow",
            "shadowed/facade/leaf",
            "facade::leaf()",
        ),
        ("nested_alias/call_nested", "leaf", "local_facade::leaf()"),
        (
            "nested_alias/call_relative",
            "nested_alias/leaf",
            "self::leaf()",
        ),
        (
            "inactive_alias/call_active",
            "inactive_alias/disabled/leaf",
            "disabled::leaf()",
        ),
        (
            "glob_shadowed/call_glob",
            "imports/facade/leaf",
            "facade::leaf()",
        ),
    ] {
        let function = owner.rsplit('/').next().expect("authored function");
        let function_start = source
            .find(&format!("fn {function}("))
            .expect("function source");
        let invocation_start = source[function_start..]
            .find(invocation)
            .expect("invocation");
        let start = u64::try_from(function_start + invocation_start).expect("source offset");
        let end = start + u64::try_from(invocation.len()).expect("invocation length");
        let calls = publication
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::Calls] && edge.from.as_str() == format!("{ROOT}/{owner}")
            })
            .collect::<Vec<_>>();
        let [call] = calls.as_slice() else {
            panic!("one authored call for {owner}");
        };
        assert_eq!(call.to.as_str(), format!("{ROOT}/{target}"));
        let site = call.occurrence.as_ref().expect("authored invocation");
        assert_eq!(site.unit, unit.unit);
        assert_eq!((site.range.start, site.range.end), (start, end));
    }
    assert!(!publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge
                .from
                .as_str()
                .ends_with("/external_shadowed/call_external")
            && edge.to.as_str() == format!("{ROOT}/thread/yield_now")
    }));
}

#[test]
fn a_nested_rust_extern_crate_binding_does_not_enter_the_crate_prelude() {
    use rift_protocol::read::EdgeKind;

    let source = r"
pub fn leaf() -> u8 { 7 }
pub mod enclosing {
    extern crate self as private_facade;
    pub mod child {
        pub fn caller() -> u8 { private_facade::leaf() }
    }
}";
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    assert!(publication.declarations.iter().any(|declaration| {
        declaration.symbol.as_str() == format!("{ROOT}/enclosing/child/caller")
    }));
    assert!(!publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls] && edge.from.as_str().ends_with("/enclosing/child/caller")
    }));
}

const RUST_RUNTIME_ALIAS_SOURCE: &str = r"
pub extern crate std as platform;
pub fn caller() { platform::thread::yield_now(); }

pub mod shadowed {
    mod platform {
        pub mod thread {
            pub fn yield_now() -> u16 { 42 }
        }
    }
    pub fn caller() -> u16 { platform::thread::yield_now() }
}
";
const RUST_NO_STD_RUNTIME_ALIAS_SOURCE: &str = r"
#![no_std]
extern crate core as platform;
pub fn caller() -> u8 { platform::hint::black_box(1u8) }
";

#[test]
fn package_publishes_stdlib_imports_and_calls_through_rust_extern_crate_aliases() {
    use rift_protocol::read::EdgeKind;

    const RUNTIME: &str = "rift://symbol/stdlib/rust/rust";
    for (source, runtime, written_call, target) in [
        (
            RUST_RUNTIME_ALIAS_SOURCE,
            "std",
            "platform::thread::yield_now()",
            "thread/yield_now",
        ),
        (
            RUST_NO_STD_RUNTIME_ALIAS_SOURCE,
            "core",
            "platform::hint::black_box(1u8)",
            "hint/black_box",
        ),
    ] {
        let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
        let publication = analysis.publication();
        let [unit] = publication.units.as_slice() else {
            panic!("one authored library unit");
        };
        let root_caller = format!("{ROOT}/caller");
        let runtime_target = format!("{RUNTIME}/{runtime}/{target}");
        for (kind, from, to, authored) in [
            (
                EdgeKind::Imports,
                ROOT,
                format!("{RUNTIME}/{runtime}"),
                runtime,
            ),
            (
                EdgeKind::Calls,
                root_caller.as_str(),
                runtime_target,
                written_call,
            ),
        ] {
            let written = if kind == EdgeKind::Imports {
                source.find("extern crate ").expect("authored crate") + "extern crate ".len()
            } else {
                source.find(authored).expect("authored invocation")
            };
            let start = u64::try_from(written).expect("source offset");
            let end = start + u64::try_from(authored.len()).expect("source length");
            assert!(
                publication.edges.iter().any(|edge| {
                    edge.kinds == [kind]
                        && edge.from.as_str() == from
                        && edge.to.as_str() == to
                        && edge.occurrence.as_ref().is_some_and(|site| {
                            site.unit == unit.unit
                                && site.range.start == start
                                && site.range.end == end
                        })
                }),
                "{from} to {to} at {start}"
            );
        }
        if runtime == "std" {
            let call = publication
                .edges
                .iter()
                .find(|edge| {
                    edge.kinds == [EdgeKind::Calls]
                        && edge.from.as_str().ends_with("/shadowed/caller")
                })
                .expect("local call");
            assert_eq!(
                call.to.as_str(),
                format!("{ROOT}/shadowed/platform/thread/yield_now")
            );
        }
    }
}

#[test]
fn package_refuses_unobserved_rust_extern_crate_aliases() {
    use rift_protocol::read::EdgeKind;

    let source = r"
#![no_std]
#[cfg(any())]
extern crate std as disabled;
extern crate untracked as platform;
pub fn inactive() { disabled::thread::yield_now(); }
pub fn unknown() { platform::thread::yield_now(); }
";
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    for caller in ["/inactive", "/unknown"] {
        assert!(
            publication
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().ends_with(caller))
        );
        assert!(!publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls] && edge.from.as_str().ends_with(caller)
        }));
    }
    assert!(
        !publication
            .edges
            .iter()
            .any(|edge| edge.kinds == [EdgeKind::Imports])
    );
}

#[test]
fn package_prefers_written_extern_bindings_to_implicit_runtime_paths() {
    use rift_protocol::read::EdgeKind;

    const SELF_AS_CORE: &str = r"
pub extern crate self as core;
pub mod beacon { pub fn run() -> u8 { 42 } }
pub fn caller() -> u8 { core::beacon::run() }
";
    const CORE_AS_STD: &str = r"
#![no_std]
pub extern crate core as std;
pub fn caller() { std::hint::spin_loop(); }
";
    const LOCAL_SHADOW: &str = r"
pub mod core { pub mod beacon { pub fn run() -> u8 { 42 } } }
pub fn caller() -> u8 { core::beacon::run() }
";
    const INACTIVE_OR_OUTSIDE: &str = r"
#[cfg(any())]
pub extern crate self as core;
pub mod other { extern crate self as core; }
pub fn caller() { core::hint::spin_loop(); }
";
    const NESTED_CHILD_RUNTIME: &str = r"
pub mod outer {
    extern crate self as core;
    pub mod child { pub fn caller() { core::hint::spin_loop(); } }
}
";
    const NESTED_CHILD_ROOT_ALIAS: &str = r"
pub extern crate self as core;
pub mod beacon { pub fn run() -> u8 { 42 } }
pub mod outer {
    extern crate std as core;
    pub mod child { pub fn caller() -> u8 { core::beacon::run() } }
}
";
    const RUNTIME: &str = "rift://symbol/stdlib/rust/rust/core/hint/spin_loop";
    let package_target = format!("{ROOT}/beacon/run");
    let shadow_target = format!("{ROOT}/core/beacon/run");
    for (source, caller, target, invocation) in [
        (
            SELF_AS_CORE,
            "caller",
            package_target.as_str(),
            "core::beacon::run()",
        ),
        (CORE_AS_STD, "caller", RUNTIME, "std::hint::spin_loop()"),
        (
            LOCAL_SHADOW,
            "caller",
            shadow_target.as_str(),
            "core::beacon::run()",
        ),
        (
            INACTIVE_OR_OUTSIDE,
            "caller",
            RUNTIME,
            "core::hint::spin_loop()",
        ),
        (
            NESTED_CHILD_RUNTIME,
            "outer/child/caller",
            RUNTIME,
            "core::hint::spin_loop()",
        ),
        (
            NESTED_CHILD_ROOT_ALIAS,
            "outer/child/caller",
            package_target.as_str(),
            "core::beacon::run()",
        ),
    ] {
        let caller = format!("{ROOT}/{caller}");
        let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
        let publication = analysis.publication();
        let [unit] = publication.units.as_slice() else {
            panic!("one authored library unit");
        };
        let calls = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls] && edge.from.as_str() == caller)
            .collect::<Vec<_>>();
        let [edge] = calls.as_slice() else {
            panic!("one captured Call for {invocation}");
        };
        assert_eq!(edge.to.as_str(), target);
        let start = source.find(invocation).expect("written call");
        let site = edge.occurrence.as_ref().expect("physical call range");
        assert_eq!(site.unit, unit.unit);
        assert_eq!(
            site.range.start,
            u64::try_from(start).expect("start offset")
        );
        assert_eq!(
            site.range.end,
            u64::try_from(start + invocation.len()).expect("end offset")
        );
    }
}

#[test]
fn package_refuses_a_runtime_spelling_owned_by_an_unobserved_extern_crate() {
    use rift_protocol::read::EdgeKind;

    let source = "extern crate untracked as alloc;\npub fn caller() { alloc::vec::Vec::new(); }";
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    let caller = format!("{ROOT}/caller");
    assert!(
        publication
            .declarations
            .iter()
            .any(|row| row.symbol.as_str() == caller)
    );
    assert!(
        !publication
            .edges
            .iter()
            .any(|edge| edge.kinds == [EdgeKind::Calls] && edge.from.as_str() == caller)
    );
}

#[test]
fn disjoint_same_name_extern_bindings_keep_the_ordinary_runtime_call() {
    use std::fmt::Write as _;

    use rift_protocol::read::EdgeKind;

    const CALLER: &str = "pub fn caller() { core::hint::spin_loop(); }";
    let mut source = String::new();
    for ordinal in 0..48 {
        writeln!(
            source,
            "pub mod sibling{ordinal} {{ extern crate self as core; }}"
        )
        .expect("authored sibling module");
    }
    let caller_start = source.len();
    source.push_str(CALLER);
    let analysis = analyze_rust_root(vec![("src/lib.rs", &source)], MANIFEST);
    let publication = analysis.publication();
    let caller = format!("{ROOT}/caller");
    let calls = publication
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls] && edge.from.as_str() == caller)
        .collect::<Vec<_>>();
    let [call] = calls.as_slice() else {
        panic!("one ordinary runtime Call");
    };
    assert_eq!(
        call.to.as_str(),
        "rift://symbol/stdlib/rust/rust/core/hint/spin_loop"
    );
    let site = call.occurrence.as_ref().expect("physical call range");
    let start = caller_start + CALLER.find("core::").expect("written path");
    let end = start + "core::hint::spin_loop()".len();
    assert_eq!(
        (site.range.start, site.range.end),
        (
            u64::try_from(start).expect("start offset"),
            u64::try_from(end).expect("end offset")
        )
    );
}

#[test]
fn package_publishes_direct_macro_export_visibility() {
    let source = "#[macro_export]\nmacro_rules! plain_export { () => { 8u8 }; }\n\
                  #[macro_export (/* comment */ local_inner_macros, )]\n\n\
                  // comment\nmacro_rules! qualified_export { () => { 7u8 }; }\n\
                  #[r#macro_export(local_inner_macros)]\n\
                  macro_rules! raw_attribute { () => { 5u8 }; }\n\
                  #[macro_export(r#local_inner_macros)]\n\
                  macro_rules! raw_argument { () => { 6u8 }; }\n\
                  #[cfg_attr(any(), macro_export(local_inner_macros))]\n\
                  macro_rules! conditional { () => { 9u8 }; }\n\
                  macro_rules! private_macro { () => { 0u8 }; }\n";
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    publication.validate_facts().expect("valid package facts");
    let [unit] = publication.units.as_slice() else {
        panic!("one published source unit");
    };
    let macros = publication
        .declarations
        .iter()
        .filter(|row| row.syntax_element_kind.as_deref() == Some("macro_definition"))
        .collect::<Vec<_>>();
    assert_eq!(macros.len(), 6);
    for (name, public) in [
        ("plain_export", true),
        ("qualified_export", true),
        ("raw_attribute", true),
        ("raw_argument", true),
        ("conditional", false),
        ("private_macro", false),
    ] {
        let id = format!("{ROOT}/{name}");
        let declaration = macros
            .iter()
            .find(|row| row.symbol.as_str() == id)
            .expect("published macro declaration");
        assert_eq!(declaration.unit, unit.unit);
        assert_eq!(declaration.public, public, "{name}");
    }
}

#[test]
fn lint_expect_keeps_rust_module_declarations_and_physical_call_targets() {
    use rift_core::ReferenceRole;

    let source = r#"
#[expect(non_snake_case)]
mod child;

#[cfg(any())]
#[path = "inactive.rs"]
#[expect(non_snake_case)]
mod chosen;

#[cfg(all())]
#[path = "active.rs"]
#[expect(non_snake_case)]
mod chosen;

mod inline {
    #![expect(non_snake_case)]
    pub fn WideName() -> u16 { 8 }
}

#[rustfmt::skip]
mod opaque;

#[cfg_attr(all(), path = "elsewhere.rs")]
mod conditional;

#[allow(non_snake_case)]
fn WideName() -> u8 { 2 }

pub fn run() -> (u8, u8, u8, u16) {
    (child::WideName(), WideName(), chosen::WideName(), inline::WideName())
}
"#;
    let (publication, files, semantics, _) = analyze_rust_root(
        vec![
            ("src/lib.rs", source),
            ("src/child.rs", "pub fn WideName() -> u8 { 3 }"),
            ("src/active.rs", "pub fn WideName() -> u8 { 5 }"),
            ("src/inactive.rs", "pub fn WideName() -> u16 { 9 }"),
            ("src/opaque.rs", "pub fn OpaqueName() {}"),
            ("src/elsewhere.rs", "pub fn ElsewhereName() {}"),
            ("src/conditional.rs", "pub fn DecoyName() {}"),
        ],
        MANIFEST,
    )
    .into_parts();
    publication.validate_facts().expect("valid package facts");
    let authority = |unit: &rift_core::SourceUnitId| {
        files
            .iter()
            .find(|file| file.placement().unit() == unit)
            .expect("known source unit")
            .file()
            .path()
            .as_str()
    };
    let calls = semantics
        .static_occurrences()
        .iter()
        .filter(|row| row.role == ReferenceRole::Call)
        .map(|row| {
            let span = row.binding.range();
            let begin = usize::try_from(span.start()).expect("call start");
            let end = usize::try_from(span.end()).expect("call end");
            let target = row.target_binding.as_ref().expect("physical target");
            assert_eq!(authority(row.binding.unit()), "src/lib.rs");
            (&source[begin..end], authority(target.unit()))
        })
        .collect::<Vec<_>>();
    for (written, owner) in [
        ("child::WideName()", "src/child.rs"),
        ("WideName()", "src/lib.rs"),
        ("chosen::WideName()", "src/active.rs"),
        ("inline::WideName()", "src/lib.rs"),
    ] {
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.0 == written)
                .collect::<Vec<_>>(),
            [&(written, owner)]
        );
    }
    assert!(publication.declarations.iter().all(|declaration| {
        let name = declaration.symbol.as_str();
        !name.ends_with("/OpaqueName") && !name.ends_with("/DecoyName")
    }));
    assert!(publication.declarations.iter().any(|declaration| {
        declaration
            .symbol
            .as_str()
            .ends_with("/conditional/ElsewhereName")
            && declaration.unit.as_str().ends_with("src/elsewhere.rs")
    }));
}

fn assert_root_declaration(
    publication: &rift_protocol::index::PackagePublication,
    unit: &rift_core::SourceUnitId,
    source: &str,
) {
    let declaration = publication
        .declarations
        .iter()
        .find(|declaration| declaration.symbol.as_str() == ROOT)
        .expect("physical library root");
    assert_eq!(declaration.unit.0, unit.to_string());
    assert_eq!(
        declaration.range,
        TextRange {
            start: 0,
            end: source.len() as u64
        }
    );
    assert_eq!(declaration.source, source);
    assert_eq!(
        declaration.syntax_element_kind.as_deref(),
        Some("source_file")
    );
}

#[test]
fn library_root_publishes_stored_external_import_and_preserves_authored_rows() {
    let (publication, files, semantics, _) = analyze_rust_root(
        vec![("src/lib.rs", SOURCE), ("src/sibling.rs", SIBLING)],
        MANIFEST,
    )
    .into_parts();
    let module = CoreSymbolId::new(ROOT.to_owned()).expect("library identity");
    let source = files
        .iter()
        .find(|source| source.file().path().as_str() == "src/lib.rs")
        .expect("library root");
    let unit = source.placement().unit();
    assert_eq!(source.placement().logical_identity("."), Some(&module));
    assert_eq!(semantics.edges().edge_module(unit), Some(&module));
    let mut objects = publication
        .objects
        .iter()
        .filter(|object| object.id.as_ref().is_some_and(|id| id.as_str() == ROOT));
    let object = objects.next().expect("published library root");
    assert!(objects.next().is_none());
    assert_eq!(object.kinds, [SymbolKind::Module]);
    assert_eq!(object.origin.source_kind, SourceKind::Authored);
    assert!(object.origin.location.is_some());
    assert_root_declaration(&publication, unit, SOURCE);
    let root_imports = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.from.as_str() == ROOT
                && edge.kinds == [EdgeKind::Imports]
                && edge.to.as_str().starts_with("rift://symbol/stdlib/")
        })
        .collect::<Vec<_>>();
    assert_eq!(root_imports.len(), 1);
    let external = root_imports[0];
    assert_eq!(external.to.as_str(), DEBUG);
    assert_eq!(external.provider, EdgeProvider::Static);
    assert_eq!(external.derivation, EdgeDerivation::Syntax);
    let occurrence = external.occurrence.as_ref().expect("written import span");
    assert_eq!(occurrence.unit.0, unit.to_string());
    assert_eq!(occurrence.range, TextRange { start: 10, end: 25 });
    let captured = semantics.edges().outgoing(&module);
    assert!(captured.iter().any(|edge| &edge.to_protocol() == external));
    assert!(captured.iter().any(|edge| {
        edge.kind() == EdgeKind::Imports && edge.to().as_str() == format!("{ROOT}/sibling/Local")
    }));
    assert_eq!(
        semantics.static_targets_omitted().get(&EdgeKind::Imports),
        Some(&1)
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/run")
            && edge.to.as_str() == VEC_NEW
            && edge.kinds == [EdgeKind::Calls]
    }));
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/PublicType")
            && edge.to.as_str() == format!("{ROOT}/sibling/AliasSource")
            && edge.kinds == [EdgeKind::Aliases]
    }));
    assert!(publication.objects.iter().any(|object| {
        object
            .id
            .as_ref()
            .is_some_and(|id| id.as_str() == format!("{ROOT}/sibling"))
            && object.origin.source_kind == SourceKind::Authored
    }));
    let sibling = files
        .iter()
        .find(|source| source.file().path().as_str() == "src/sibling.rs")
        .expect("declared module");
    assert!(sibling.placement().logical_identity(".").is_none());
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == format!("{ROOT}/sibling")
            && edge.to.as_str() == "rift://symbol/stdlib/rust/rust/std/fmt/Write"
            && edge.kinds == [EdgeKind::Imports]
            && edge.occurrence.as_ref().is_some_and(|span| {
                span.unit.0 == sibling.placement().unit().to_string()
                    && span.range == TextRange { start: 4, end: 19 }
            })
    }));
    assert!(
        publication
            .edges
            .iter()
            .all(|edge| !edge.to.as_str().contains("untracked"))
    );
    publication.validate_facts().expect("covered publication");
}

#[test]
fn library_root_requires_a_checked_source_even_when_edges_are_available() {
    let cycle = "mod lib;\nuse std::fmt::Debug;\npub fn run() {}";
    let malformed = "use std::fmt::Debug;\npub fn run() {}\npub fn broken( {";
    let missing = format!("{MANIFEST}\n[lib]\npath='src/absent.rs'\n");
    let binary = format!("{MANIFEST}\nautolib=false\n");
    let local = "pub mod sibling;\nuse crate::sibling::Local;\npub fn run() {}";
    let unresolved = "use untracked::Thing;\npub fn run() {}";
    for (path, source, manifest, anchored) in [
        ("src/lib.rs", cycle, MANIFEST, false),
        ("src/lib.rs", malformed, MANIFEST, false),
        ("src/lib.rs", SOURCE, missing.as_str(), false),
        ("src/main.rs", SOURCE, binary.as_str(), false),
        ("src/lib.rs", local, MANIFEST, true),
        ("src/lib.rs", unresolved, MANIFEST, true),
    ] {
        let (publication, files, _, _) =
            analyze_rust_root(vec![(path, source), ("src/sibling.rs", SIBLING)], manifest)
                .into_parts();
        let root = files
            .iter()
            .find(|file| file.file().path().as_str() == path)
            .expect("candidate source");
        assert_eq!(
            root.placement().logical_identity(".").is_some(),
            anchored,
            "{path}: {source}"
        );
        assert_eq!(
            publication
                .objects
                .iter()
                .any(|object| object.id.as_ref().is_some_and(|id| id.as_str() == ROOT)),
            anchored
        );
        assert_eq!(
            publication
                .edges
                .iter()
                .any(|edge| edge.from.as_str() == ROOT && edge.kinds == [EdgeKind::Contains]),
            anchored
        );
        assert!(
            !publication
                .edges
                .iter()
                .any(|edge| edge.to.as_str().contains("untracked"))
        );
    }
}

#[test]
fn package_publishes_proven_macro_calls_at_authored_invocations() {
    let source = r"
mod exported;
macro_rules! local { () => { 1u8 }; }
pub fn ordinary() -> u8 { 2 }
pub fn all() -> u8 { ordinary() + crate::exported!() + local!() }
";
    let other = "#[macro_export]\nmacro_rules! exported { () => { 3u8 }; }\n";
    let publication = analyze_rust_root(
        vec![("src/lib.rs", source), ("src/exported.rs", other)],
        MANIFEST,
    );
    let publication = publication.publication();
    let caller = format!("{ROOT}/all");
    let calls = publication
        .edges
        .iter()
        .filter(|edge| edge.from.as_str() == caller && edge.kinds == [EdgeKind::Calls])
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 3);
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "src/lib.rs")
        .expect("source unit");
    for (authored, target) in [
        ("ordinary()", "ordinary"),
        ("crate::exported!()", "exported/exported"),
        ("local!()", "local"),
    ] {
        let start = source.rfind(authored).expect("invocation site");
        let range = TextRange {
            start: u64::try_from(start).expect("invocation byte start"),
            end: u64::try_from(start + authored.len()).expect("invocation byte end"),
        };
        assert!(calls.iter().any(|edge| {
            edge.to.as_str() == format!("{ROOT}/{target}")
                && edge.occurrence.as_ref().is_some_and(|occurrence| {
                    occurrence.unit == unit.unit && occurrence.range == range
                })
        }));
    }
}

const NAMED_MACRO_IMPORT_ROOT: &str = r"
mod definitions;
mod opaque;
mod consumer;
use crate::root_export as root_import;
";

const NAMED_MACRO_IMPORT_CONSUMER: &str = r"
use crate::definitions::{source, source as alias};
use crate::root_export as exported;
use crate::local_export as local;
use crate::definitions::hidden;
use crate::opaque::uncertain;
pub fn before_shadow() -> u8 { alias!() }
macro_rules! alias { () => { 5u8 }; }
pub fn after_shadow() -> u8 { alias!() }
";

const NAMED_MACRO_IMPORT_DEFINITIONS: &str = r"
macro_rules! source { () => { 2u8 }; }
pub(crate) use source;
macro_rules! hidden { () => { 3u8 }; }
use hidden;
#[macro_export]
macro_rules! root_export { () => { 4u8 }; }
#[macro_export(local_inner_macros)]
macro_rules! local_export { () => { 9u8 }; }
";

const NAMED_MACRO_IMPORT_OPAQUE: &str = r"
macro_rules! uncertain { () => { 6u8 }; }
macro_rules! introduce { () => { const OTHER: u8 = 7; }; }
introduce!();
pub(crate) use uncertain;
";

#[test]
fn named_macro_imports_keep_the_written_path_and_defining_module() {
    let analysis = analyze_rust_root(
        vec![
            ("src/lib.rs", NAMED_MACRO_IMPORT_ROOT),
            ("src/definitions.rs", NAMED_MACRO_IMPORT_DEFINITIONS),
            ("src/opaque.rs", NAMED_MACRO_IMPORT_OPAQUE),
            ("src/consumer.rs", NAMED_MACRO_IMPORT_CONSUMER),
        ],
        MANIFEST,
    );
    let publication = analysis.publication();
    let path_for = |unit: &rift_protocol::read::SourceUnitId| {
        publication
            .units
            .iter()
            .find(|source| &source.unit == unit)
            .expect("source for the occurrence")
            .path
            .0
            .as_str()
    };
    let root_prefix = format!("{ROOT}/");
    let mut imports = publication
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Imports])
        .filter_map(|edge| {
            let occurrence = edge.occurrence.as_ref()?;
            let path = path_for(&occurrence.unit);
            let source = match path {
                "src/consumer.rs" => NAMED_MACRO_IMPORT_CONSUMER,
                "src/definitions.rs" => NAMED_MACRO_IMPORT_DEFINITIONS,
                _ => return None,
            };
            let start = usize::try_from(occurrence.range.start).expect("occurrence start");
            let end = usize::try_from(occurrence.range.end).expect("occurrence end");
            let written = &source[start..end];
            let target = edge
                .to
                .as_str()
                .strip_prefix(root_prefix.as_str())
                .expect("same crate macro");
            Some((path, written, target))
        })
        .collect::<Vec<_>>();
    imports.sort_unstable();
    assert_eq!(
        imports,
        [
            (
                "src/consumer.rs",
                "crate::local_export",
                "definitions/local_export"
            ),
            (
                "src/consumer.rs",
                "crate::root_export",
                "definitions/root_export"
            ),
            ("src/consumer.rs", "source", "definitions/source"),
            ("src/consumer.rs", "source", "definitions/source"),
            ("src/definitions.rs", "hidden", "definitions/hidden"),
            ("src/definitions.rs", "source", "definitions/source"),
        ]
    );

    let root_macro = format!("{ROOT}/definitions/root_export");
    assert!(analysis.semantics.edges().edges().any(|edge| {
        edge.from().as_str() == ROOT
            && edge.to().as_str() == root_macro
            && edge.kind() == EdgeKind::Imports
    }));
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == ROOT
            && edge.to.as_str() == root_macro
            && edge.kinds == [EdgeKind::Imports]
    }));
    let before = format!("{ROOT}/consumer/before_shadow");
    let after = format!("{ROOT}/consumer/after_shadow");
    let target = format!("{ROOT}/consumer/alias");
    assert!(
        !publication
            .edges
            .iter()
            .any(|edge| edge.from.as_str() == before && edge.kinds == [EdgeKind::Calls])
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str() == after && edge.to.as_str() == target && edge.kinds == [EdgeKind::Calls]
    }));
    assert_eq!(
        analysis
            .semantics
            .static_targets_omitted()
            .get(&EdgeKind::Imports),
        Some(&3)
    );
}

#[test]
fn authored_library_root_contains_items_without_an_external_import() {
    let source = "pub fn top() {}
pub struct Thing { value: u8 }
pub mod nested { pub fn child() {} }
";
    let manifest = format!(
        "{MANIFEST}
[lib]
path='sources/library.rs'
"
    );
    let (publication, files, _, _) = analyze_rust_root(
        vec![
            ("sources/library.rs", source),
            ("src/main.rs", "fn main() {}"),
        ],
        &manifest,
    )
    .into_parts();
    let root = files
        .iter()
        .find(|file| file.file().path().as_str() == "sources/library.rs")
        .expect("selected library");
    assert!(publication.declarations.iter().any(|row| {
        row.symbol.as_str() == ROOT
            && row.unit.0 == root.placement().unit().to_string()
            && row.range
                == (TextRange {
                    start: 0,
                    end: source.len() as u64,
                })
    }));
    for (name, written) in [("top", "top"), ("Thing", "Thing"), ("nested", "nested")] {
        let target = format!("{ROOT}/{name}");
        let start = source.find(written).expect("top-level written name") as u64;
        let end = start + written.len() as u64;
        assert!(publication.edges.iter().any(|edge| {
            edge.from.as_str() == ROOT
                && edge.to.as_str() == target
                && edge.kinds == [EdgeKind::Contains]
                && edge.occurrence.as_ref().is_some_and(|span| {
                    span.unit.0 == root.placement().unit().to_string()
                        && span.range == TextRange { start, end }
                })
        }));
    }
    assert!(
        publication
            .edges
            .iter()
            .all(|edge| { edge.from.as_str() != ROOT || !edge.to.as_str().ends_with("/main") })
    );
    publication.validate_facts().expect("covered publication");
}

#[test]
fn authored_crate_path_tokens_reference_the_selected_library_root() {
    const LIBRARY: &str = "
pub mod child;
pub fn leaf() {}
pub fn call() { crate::leaf(); }
#[cfg(any())]
pub fn inactive() { crate::leaf(); }
pub fn private() {}
";
    const CHILD: &str = "
use crate::leaf as imported;
pub fn run() { crate::leaf(); imported(); }
";
    let (publication, files, _, _) = analyze_rust_root(
        vec![
            ("src/lib.rs", LIBRARY),
            ("src/child.rs", CHILD),
            ("src/main.rs", "fn main() { crate::missing(); }"),
        ],
        MANIFEST,
    )
    .into_parts();
    for (path, source, expected) in [
        (
            "src/lib.rs",
            LIBRARY,
            vec![LIBRARY.find("crate::").expect("written qualifier")],
        ),
        (
            "src/child.rs",
            CHILD,
            CHILD
                .match_indices("crate::")
                .map(|(offset, _)| offset)
                .collect::<Vec<_>>(),
        ),
    ] {
        let unit = files
            .iter()
            .find(|file| file.file().path().as_str() == path)
            .expect("selected source")
            .placement()
            .unit()
            .to_string();
        let mut actual = publication
            .edges
            .iter()
            .filter_map(|edge| {
                let occurrence = edge.occurrence.as_ref()?;
                (edge.to.as_str() == ROOT
                    && edge.kinds == [EdgeKind::Reads]
                    && occurrence.unit.0 == unit)
                    .then_some(occurrence.range.clone())
            })
            .collect::<Vec<_>>();
        actual.sort_by_key(|range| range.start);
        let expected = expected
            .into_iter()
            .map(|start| TextRange {
                start: start as u64,
                end: (start + 5) as u64,
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{source}");
    }
    assert!(publication.edges.iter().any(|edge| {
        edge.from.as_str().ends_with("/call")
            && edge.to.as_str().ends_with("/leaf")
            && edge.kinds == [EdgeKind::Calls]
    }));
    assert!(publication.edges.iter().all(|edge| {
        edge.to.as_str() != ROOT
            || edge
                .occurrence
                .as_ref()
                .is_none_or(|span| !span.unit.0.ends_with("/src/main.rs"))
    }));
}

#[test]
fn authored_crate_visibility_references_the_selected_library_root() {
    let source = "pub(crate) fn leaf() {}
pub(in crate) fn other() {}
pub fn caller() { leaf(); other(); }
";
    let analysis = analyze_rust_root(vec![("src/lib.rs", source)], MANIFEST);
    let publication = analysis.publication();
    let mut ranges = publication
        .edges
        .iter()
        .filter_map(|edge| {
            let span = edge.occurrence.as_ref()?;
            (edge.to.as_str() == ROOT && edge.kinds == [EdgeKind::Reads])
                .then_some(span.range.clone())
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);
    assert_eq!(
        ranges,
        source
            .match_indices("crate")
            .map(|(start, _)| TextRange {
                start: start as u64,
                end: (start + 5) as u64
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls])
            .count(),
        2
    );
}

#[test]
fn package_rust_cfg_attr_path_keeps_only_the_selected_file_owner() {
    let native = "#[cfg_attr(feature = \"native\", path = \"platform.rs\")] pub mod selected;
pub fn call() { selected::entry(); }";
    let manifest =
        |enabled| format!("{MANIFEST}\n[features]\ndefault = [{enabled}]\nnative = []\n");
    let sources = |source| {
        vec![
            ("src/lib.rs", source),
            ("src/platform.rs", "pub fn entry() {}"),
            ("src/selected.rs", "pub fn entry() {}"),
        ]
    };
    for (feature, physical) in [("\"native\"", "src/platform.rs"), ("", "src/selected.rs")] {
        let selected = analyze_rust_root(sources(native), &manifest(feature));
        let declaration = selected
            .publication()
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with("/selected/entry"))
            .expect("selected child declaration");
        assert!(declaration.unit.as_str().ends_with(physical));
        assert!(
            selected
                .publication()
                .edges
                .iter()
                .any(|row| row.kinds == [EdgeKind::Calls]
                    && row.from.as_str().ends_with("/call")
                    && row.to == declaration.symbol)
        );
    }
    for refused in [
        "#[cfg_attr(unknown, path = \"platform.rs\")] pub mod selected;",
        "#[cfg_attr(feature = \"native\", path = \"platform.rs\", path = \"selected.rs\")] pub mod selected;",
        "#[cfg_attr(feature = \"native\", path = \"platform.rs\", transform)] pub mod selected;",
    ] {
        let result = analyze_rust_root(sources(refused), &manifest("\"native\""));
        assert!(
            result
                .publication()
                .declarations
                .iter()
                .all(|row| !row.symbol.as_str().ends_with("/selected/entry")),
            "{refused}"
        );
    }
}
