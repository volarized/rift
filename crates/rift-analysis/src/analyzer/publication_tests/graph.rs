//! Captured graph projection, source occurrences, and independent coverage.

use super::*;
use rift_core::{
    Contribution, ContributionEdge, ContributionKey, ContributionReference, DeclarationBinding,
    IndexRevision, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
    SourceApplicability, SourceRange, SourceRevision, TreeRevision,
};
use rift_protocol::read::{EdgeDerivation, EdgeKind, ExactKind};
use rift_provider::{Normalizer, ProviderPublication, PublicationLimits, PublicationSet};

const SOURCE: &str = "// é\npub fn start() { finish(); finish(); }\npub fn finish() {}\n";

#[test]
fn package_collects_python_generic_aliases_in_their_own_scope() {
    let source = "class T: pass\nclass Bound: pass\n\
                  type Plain = T\n\
                  type Values[T] = T\n\
                  type WithDefault[T = int] = T\n\
                  type Bounded[\n    T: Bound,  # bound of the alias\n] = list[T]\n\
                  def accept(value: Values[int]) -> Values[int]: return value\n";
    let result = publication(&[("click.py", source)], None, &["click"]);
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let identity = |name| format!("{module}/{name}");
    for alias in ["Plain", "Values", "WithDefault", "Bounded"] {
        let name = identity(alias);
        assert!(
            result
                .objects
                .iter()
                .any(|object| { object.id.as_ref().is_some_and(|id| id.as_str() == name) }),
            "{alias}"
        );
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Contains]
                    && edge.from.as_str() == module
                    && edge.to.as_str() == name
            }),
            "{alias}"
        );
    }
    let module_type = identity("T");
    let plain_alias = identity("Plain");
    let uses = result
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::HasType] && edge.to.as_str() == module_type)
        .map(|edge| edge.from.as_str())
        .collect::<Vec<_>>();
    assert_eq!(uses, [plain_alias.as_str()]);
    for (source, target) in [("Bounded", "Bound"), ("accept", "Values")] {
        let from = identity(source);
        let to = identity(target);
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str() == from
                    && edge.to.as_str() == to
            }),
            "{source} -> {target}"
        );
    }
}

#[test]
fn package_preserves_python_edges_beside_type_parameter_defaults() {
    let prefix = "import math\ndef leaf(): pass\ndef f(): leaf()\n";
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let report = |suffix| {
        let source = format!("{prefix}{suffix}");
        publication(&[("click.py", &source)], None, &["click"])
    };
    let f = format!("{module}/f");
    let leaf = format!("{module}/leaf");
    for (suffix, bound) in [
        ("class A[T: int]: pass\n", true),
        ("class A[T = int]: pass\n", false),
        ("class A[T: int = int]: pass\n", true),
        (
            "def identity[T = leaf()](value: T) -> T: return value\n",
            false,
        ),
        ("type Values[T = int] = list[T]\n", false),
        (
            "class A[*Ts = *tuple[int, str], **P = [int, str]]: pass\n",
            false,
        ),
    ] {
        let result = report(suffix);
        let calls = result
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls])
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(calls, [(f.as_str(), leaf.as_str())], "{suffix}");
        assert!(
            !result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.from.as_str().ends_with("/identity")
                    && edge.to.as_str() == leaf
            }),
            "{suffix}"
        );
        assert_eq!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str().ends_with("/A")
                    && edge.to.as_str().ends_with("/builtins/int")
            }),
            bound,
            "{suffix}"
        );
        for member in [&f, &leaf] {
            assert!(
                result.edges.iter().any(|edge| {
                    edge.kinds == [EdgeKind::Contains]
                        && edge.from.as_str() == module
                        && edge.to.as_str() == member.as_str()
                }),
                "{suffix}"
            );
        }
        assert!(
            result
                .edges
                .iter()
                .any(|edge| { edge.kinds == [EdgeKind::Imports] && edge.from.as_str() == module }),
            "{suffix}"
        );
    }
    for suffix in [
        "class A[T = : pass\n",
        "class A[41 = int]: pass\n",
        "class A[T = *tuple[int]]: pass\n",
        "class A[**P = *tuple[int]]: pass\n",
        "def invalid(value: list[T = int]): pass\n",
    ] {
        assert!(report(suffix).edges.is_empty(), "{suffix}");
    }
}

#[test]
fn package_stores_typescript_imported_type_uses_at_each_written_site() {
    use rift_protocol::read::EdgeProvider;

    let source = "import type { ZodType as Imported } from 'shape';\n\
                  export type Schema = Imported;\n\
                  export function accept(value: Imported): Imported { return value; }\n";
    let analysis = fixture::package_result_with_metadata(
        rift_syntax::ShippedLanguage::TypeScript,
        vec![("index.ts", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"index.ts","dependencies":{"shape":"npm:zod@^4"}}"#,
        )),
    )
    .expect("admitted TypeScript package");
    let (publication, files, semantics, _) = analysis.into_parts();
    let unit = files[0].placement.unit().to_string();
    let target = "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType";
    let rows = |edges: &[rift_protocol::index::StoredEdge]| {
        let mut rows = edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::HasType] && edge.to.as_str() == target)
            .map(|edge| {
                let span = edge.occurrence.as_ref().expect("written type use");
                assert_eq!(span.unit.as_str(), unit.as_str());
                let start = usize::try_from(span.range.start).expect("type use start");
                let end = usize::try_from(span.range.end).expect("type use end");
                assert_eq!(&source[start..end], "Imported");
                assert_eq!(edge.provider, EdgeProvider::Static);
                assert_eq!(edge.derivation, EdgeDerivation::Syntax);
                (edge.from.0.clone(), start)
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };
    let positions = source
        .match_indices("Imported")
        .skip(1)
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    let [alias, parameter, returns] = positions.as_slice() else {
        panic!("one alias, parameter and return type use")
    };
    let expected = [
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/Schema".to_owned(),
            *alias,
        ),
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/accept".to_owned(),
            *parameter,
        ),
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/accept".to_owned(),
            *returns,
        ),
    ];
    let stored = semantics
        .edges()
        .edges()
        .map(crate::edge::Edge::to_protocol)
        .collect::<Vec<_>>();
    assert_eq!(rows(&stored), expected);
    assert_eq!(rows(&publication.edges), expected);
}

#[test]
fn package_rust_constructors_and_fields_use_their_block_owner() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    const ROOT: &str = "pub struct Target { pub item: u8 }";
    const LOCAL: &str = "struct Target { item: u16 }";
    const READ: &str = "let value = Target { item: 7 }; value.item";
    for (body, returns, owner, definition, field) in [
        (
            format!("{{ {LOCAL} }} {READ}"),
            "u8",
            "Target",
            ROOT,
            "pub item: u8",
        ),
        (
            format!("let value = Target {{ item: 7 }}; {{ {LOCAL} }} value.item"),
            "u8",
            "Target",
            ROOT,
            "pub item: u8",
        ),
        (READ.to_owned(), "u8", "Target", ROOT, "pub item: u8"),
        (
            format!("{{ {LOCAL} {READ} }}"),
            "u16",
            "run/Target",
            LOCAL,
            "item: u16",
        ),
        (
            format!("{{ let value = Target {{ item: 7 }}; {LOCAL} value.item }}"),
            "u16",
            "run/Target",
            LOCAL,
            "item: u16",
        ),
    ] {
        let source = format!("{ROOT}\npub fn run() -> {returns} {{ {body} }}");
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", &source)], None)
                .expect("admitted Rust package");
        let (_, _, semantics, _) = analysis.into_parts();
        let written = |binding: &DeclarationBinding| {
            let range = binding.range();
            &source[offset_in(range.start())..offset_in(range.end())]
        };
        for (role, text, target, authored) in [
            (
                ReferenceRole::Call,
                "Target { item: 7 }",
                format!("/beacon/{owner}"),
                definition,
            ),
            (
                ReferenceRole::Read,
                "value.item",
                format!("/beacon/{owner}/item"),
                field,
            ),
        ] {
            let found = semantics
                .static_occurrences()
                .iter()
                .filter(|edge| edge.role == role && written(&edge.binding) == text)
                .collect::<Vec<_>>();
            let [edge] = found.as_slice() else {
                panic!("expected one {role:?} of {text} in {body}, got {found:?}");
            };
            assert!(
                matches!(&edge.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(&target)),
                "{body}: {:?}",
                edge.target
            );
            assert_eq!(
                written(edge.target_binding.as_ref().expect("physical target")),
                authored,
                "{body}"
            );
        }
    }
}

#[test]
fn package_rust_same_name_block_constructors_keep_distinct_physical_targets() {
    use rift_core::ReferenceRole;

    let source = "pub fn run() -> u16 {\n\
        let first = { struct Target { item: u8 } let value = Target { item: 1 }; value.item };\n\
        let second = { struct Target { item: u16 } let value = Target { item: 2 }; value.item };\n\
        u16::from(first) + second\n}";
    let analysis =
        fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", source)], None)
            .expect("admitted Rust package");
    let (_, files, semantics, _) = analysis.into_parts();
    let qualified = files[0]
        .file()
        .syntax()
        .symbols()
        .iter()
        .filter(|symbol| symbol.name == "Target")
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(qualified, ["run::Target", "run::Target"]);

    let written = |binding: &DeclarationBinding| {
        let range = binding.range();
        &source[offset_in(range.start())..offset_in(range.end())]
    };
    for (label, declaration) in [
        ("Target { item: 1 }", "struct Target { item: u8 }"),
        ("Target { item: 2 }", "struct Target { item: u16 }"),
    ] {
        let found = semantics
            .static_occurrences()
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Call && written(&edge.binding) == label)
            .collect::<Vec<_>>();
        let [edge] = found.as_slice() else {
            panic!("expected one Call of {label}, got {found:?}");
        };
        assert_eq!(
            written(edge.target_binding.as_ref().expect("physical target")),
            declaration
        );
    }
}

#[test]
fn package_rust_block_types_respect_import_namespaces_and_inline_modules() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    for (source, target, constructor, field) in [
        (
            r"pub mod src { pub struct Target { pub item: u8 } }

pub fn run() -> u8 {
    struct Target { item: u16 }
    {
        use crate::src::Target;
        let value = Target { item: 7 };
        value.item
    }
}",
            "src/Target",
            "pub struct Target { pub item: u8 }",
            "pub item: u8",
        ),
        (
            r"pub fn run() -> u8 {
    struct Target { item: u16 }
    mod inner {
        pub struct Target { pub item: u8 }
        pub fn read() -> u8 {
            let value = Target { item: 7 };
            value.item
        }
    }
    inner::read()
}",
            "run/inner/Target",
            "pub struct Target { pub item: u8 }",
            "pub item: u8",
        ),
        (
            r"pub mod src { pub fn maker() -> u8 { 1 } }

pub fn run() -> u16 {
    struct Target { item: u16 }
    {
        use crate::src::maker as Target;
        let value = Target { item: 7 };
        let _ = Target();
        value.item
    }
}",
            "run/Target",
            "struct Target { item: u16 }",
            "item: u16",
        ),
    ] {
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", source)], None)
                .expect("admitted Rust package");
        let (_, _, semantics, _) = analysis.into_parts();
        let written = |binding: &DeclarationBinding| {
            let range = binding.range();
            &source[offset_in(range.start())..offset_in(range.end())]
        };
        for (role, occurrence, suffix, definition) in [
            (
                ReferenceRole::Call,
                "Target { item: 7 }",
                format!("/beacon/{target}"),
                constructor,
            ),
            (
                ReferenceRole::Read,
                "value.item",
                format!("/beacon/{target}/item"),
                field,
            ),
        ] {
            let found = semantics
                .static_occurrences()
                .iter()
                .filter(|edge| edge.role == role && written(&edge.binding) == occurrence)
                .collect::<Vec<_>>();
            let [edge] = found.as_slice() else {
                panic!("expected one {role:?} of {occurrence} in {source}, got {found:?}");
            };
            assert!(
                matches!(&edge.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(&suffix)),
                "{source}: {:?}",
                edge.target,
            );
            assert_eq!(
                written(edge.target_binding.as_ref().expect("physical target")),
                definition,
                "{source}",
            );
        }
    }
}

#[test]
fn package_rust_initializer_local_types_do_not_name_the_module_type() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    let module_target =
        |id: &str| id.ends_with("/beacon/Target") || id.ends_with("/beacon/Target/item");
    for (label, source) in [
        (
            "named const",
            "pub struct Target { pub item: u8 }\npub const CHECK: u16 = {\nstruct Target { item: u16 }\nlet value = Target { item: 7 };\nvalue.item\n};\n",
        ),
        (
            "named static",
            "pub struct Target { pub item: u8 }\npub static CHECK: u16 = {\nstruct Target { item: u16 }\nlet value = Target { item: 7 };\nvalue.item\n};\n",
        ),
        (
            "tuple const",
            "pub struct Target(pub u8);\npub const CHECK: u16 = {\nstruct Target(u16);\nlet value = Target(7);\nvalue.0\n};\n",
        ),
    ] {
        let source = format!(
            "{source}\npub mod visible {{ pub struct Target {{ pub item: u8 }} }}\n\
             pub const EXPLICIT: u8 = crate::visible::Target {{ item: 1 }}.item;\n"
        );
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", &source)], None)
                .expect("admitted Rust package");
        let initializer =
            u64::try_from(source.find("CHECK:").expect("initializer")).expect("offset");
        let edges = &analysis.publication().edges;
        assert!(
            edges.iter().all(|edge| {
                !module_target(edge.to.as_str())
                    || !edge
                        .kinds
                        .iter()
                        .any(|kind| matches!(kind, EdgeKind::Calls | EdgeKind::Reads))
                    || edge
                        .occurrence
                        .as_ref()
                        .is_none_or(|site| site.range.start < initializer)
            }),
            "{label}: {edges:?}",
        );
        assert!(
            edges.iter().any(|edge| {
                edge.kinds.contains(&EdgeKind::Calls)
                    && edge.to.as_str().ends_with("/beacon/visible/Target")
            }),
            "missing explicit constructor in {label}: {edges:?}"
        );
        let (_, _, semantics, _) = analysis.into_parts();
        let leaked = semantics.static_occurrences().iter().find(|edge| {
            edge.binding.range().start() >= initializer
                && matches!(edge.role, ReferenceRole::Call | ReferenceRole::Read)
                && matches!(&edge.target, NormalizedTarget::Symbol(id) if module_target(id.as_str()))
        });
        assert!(leaked.is_none(), "{label}: {leaked:?}");
    }
}

#[test]
fn package_calls_keep_their_javascript_initializer_owners() {
    let source = "function load() { return 1; }\n\
                  export const value = load();\n\
                  export const items = [load()];\n\
                  export const object = { property: load() };\n\
                  export class Owner { field = load(); static shared = load(); }\n\
                  export function run() { const local = load(); return local; }\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls] && edge.to.as_str().ends_with("/load"))
        .map(|edge| {
            let span = edge.occurrence.as_ref().expect("written call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            assert_eq!(source.get(start..end), Some("load()"));
            (start, edge.from.as_str())
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let owners = [
        "value",
        "items",
        "object/property",
        "Owner/field",
        "Owner/shared",
        "run",
    ];
    assert_eq!(calls.len(), owners.len(), "{calls:?}");
    for ((start, owner), (expected, (position, _))) in calls.into_iter().zip(
        owners
            .into_iter()
            .zip(source.match_indices("load()").skip(1)),
    ) {
        assert_eq!(start, position);
        assert!(owner.ends_with(&format!("/beacon/{expected}")), "{owner}");
    }
}

#[test]
fn package_ecmascript_direct_class_values_name_stable_calls() {
    let source = r"
export var Named = class Inner { static make() { return 1; } };
export const Anonymous = class { static make() { return 2; } };
export class Declared { static make() { return 3; } }
export var Reassigned = class { static make() { return 4; } };
Reassigned = class { static make() { return 5; } };
var Competing = class { static make() { return 6; } };
var Competing = class { static make() { return 7; } };
export { Competing };
export function stable() { Named.make(); Anonymous.make(); new Anonymous(); Declared.make(); }
export function unsettled() { Reassigned.make(); new Reassigned(); Competing.make(); new Competing(); }
export function plain() { Anonymous(); }
export const Patched = class { static make() { return 8; } };
export function replace(replacement) { Patched.make = replacement; }
export function changed() { Patched.make(); }
";
    for (language, filename, entry) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            r#""main":"index.js""#,
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            r#""types":"index.ts""#,
        ),
    ] {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(filename, source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        let mut calls = analysis
            .publication()
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls])
            .map(|edge| {
                let span = edge.occurrence.as_ref().expect("written call");
                let start = usize::try_from(span.range.start).expect("start");
                let end = usize::try_from(span.range.end).expect("end");
                let owner = edge.from.as_str().rsplit("/beacon/").next().expect("owner");
                let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
                (start, &source[start..end], owner, target)
            })
            .collect::<Vec<_>>();
        calls.sort_unstable();
        assert_eq!(
            calls
                .into_iter()
                .map(|(_, label, owner, target)| (label, owner, target))
                .collect::<Vec<_>>(),
            [
                ("Named.make()", "stable", "Named/make"),
                ("Anonymous.make()", "stable", "Anonymous/make"),
                ("new Anonymous()", "stable", "Anonymous"),
                ("Declared.make()", "stable", "Declared/make"),
            ],
            "{filename}",
        );
    }
}

#[test]
fn package_ecmascript_class_heritage_accepts_a_stable_holder() {
    const COMMON: &str = r"
export const Held = class { static make() { return 1; } };
export class Written { static make() { return 2; } }
export class Derived extends Held {}
export class WrittenChild extends Written {}
export const inherited = () => Derived.make();
export var Replaced = class { static make() { return 3; } };
Replaced = class { static make() { return 4; } };
export class Dynamic extends Replaced {}
export function shadow(Held) { class Local extends Held {} return Local; }
";
    const TYPES: &str = r"
export interface Surface { item: number; }
export interface Refined extends Surface {}
export class Implementation implements Surface { item = 1; }
export type Name = Surface;
";
    for (language, filename, entry, typescript) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            r#""main":"index.js""#,
            false,
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            r#""types":"index.ts""#,
            true,
        ),
    ] {
        let source = if typescript {
            format!("{COMMON}{TYPES}")
        } else {
            COMMON.to_owned()
        };
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(filename, &source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        let mut expected = vec![
            ("Derived", EdgeKind::Extends, "Held", "Held"),
            ("WrittenChild", EdgeKind::Extends, "Written", "Written"),
            ("inherited", EdgeKind::Calls, "Held/make", "Derived.make()"),
        ];
        if typescript {
            expected.extend([
                ("Refined", EdgeKind::Extends, "Surface", "Surface"),
                ("Refined", EdgeKind::HasType, "Surface", "Surface"),
                ("Implementation", EdgeKind::Implements, "Surface", "Surface"),
                ("Implementation", EdgeKind::HasType, "Surface", "Surface"),
                ("Name", EdgeKind::HasType, "Surface", "Surface"),
            ]);
        }
        let mut resolved = analysis
            .publication()
            .edges
            .iter()
            .filter_map(|edge| {
                let [kind] = edge.kinds.as_slice() else {
                    return None;
                };
                if !matches!(
                    kind,
                    EdgeKind::Extends | EdgeKind::Implements | EdgeKind::HasType | EdgeKind::Calls
                ) {
                    return None;
                }
                let span = edge.occurrence.as_ref().expect("written relation");
                let start = usize::try_from(span.range.start).expect("start");
                let end = usize::try_from(span.range.end).expect("end");
                let from = edge
                    .from
                    .as_str()
                    .rsplit("/beacon/")
                    .next()
                    .expect("from symbol");
                let to = edge
                    .to
                    .as_str()
                    .rsplit("/beacon/")
                    .next()
                    .expect("to symbol");
                Some((from, *kind, to, &source[start..end]))
            })
            .collect::<Vec<_>>();
        resolved.sort_unstable();
        expected.sort_unstable();
        assert_eq!(resolved, expected, "{filename}");
    }
}

#[test]
fn package_ecmascript_stable_class_publication_keeps_declared_visibility() {
    const JS: &str = r"
export const Stable = class {
  static make() { return 1; }
  method() { return 2; }
  static #secret() { return 3; }
  #internal() { return 4; }
};
export var Mutable = class { static retired() { return 5; } };
Mutable = class { static replacement() { return 6; } };
export class Written { static make() { return 7; } static #secret() { return 8; } }
const Local = class { static make() { return 9; } };
export const localResult = Local.make();
";
    const TS: &str = r"
export const Stable = class {
  static make() { return 1; }
  method() { return 2; }
  private static secret() { return 3; }
  protected internal() { return 4; }
};
export class Written { static make() { return 7; } private static secret() { return 8; } }
";
    for (language, file, source, entry, expected) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            JS,
            r#""main":"index.js""#,
            &[
                ("Stable/make", true),
                ("Stable/method", true),
                ("Stable/%23secret", false),
                ("Stable/%23internal", false),
                ("Mutable/retired", false),
                ("Local/make", false),
                ("Written/make", true),
                ("Written/%23secret", false),
            ][..],
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            TS,
            r#""types":"index.ts""#,
            &[
                ("Stable/make", true),
                ("Stable/method", true),
                ("Stable/secret", false),
                ("Stable/internal", false),
                ("Written/make", true),
                ("Written/secret", false),
            ][..],
        ),
    ] {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(file, source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        for &(name, public) in expected {
            let address = format!("/beacon/{name}");
            let declarations = analysis
                .publication()
                .declarations
                .iter()
                .filter(|declaration| declaration.symbol.as_str().ends_with(&address))
                .map(|declaration| declaration.public)
                .collect::<Vec<_>>();
            assert_eq!(declarations, [public], "{file}: {name}");
        }
    }
}

#[test]
fn package_ecmascript_class_initializer_this_preserves_replacement_refusals() {
    let source = r"
export const Held = class {
  static make() { return 1; }
  static swap(next) { this.make = next; }
};
export class Written {
  static make() { return 2; }
  static swap(next) { this.make = next; }
}
export const Dynamic = class {
  static make() { return 3; }
  static swap(next) { function inner() { this.make = next; } return inner; }
};
export const Lexical = class {
  static make() { return 4; }
  static swap(next) { const bind = () => { this.make = next; }; bind(); }
};
export function run(next) { Held.swap(next); return Held.make(); }
export function control(next) { Written.swap(next); return Written.make(); }
export function nested(next) { Dynamic.swap(next); return Dynamic.make(); }
export function captured(next) { Lexical.swap(next); return Lexical.make(); }
";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .filter_map(|edge| {
            let span = edge.occurrence.as_ref().expect("physical call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
            let text = &source[start..end];
            text.contains('.').then_some((start, text, target))
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let roles = calls
        .into_iter()
        .map(|(_, source, target)| (source, target))
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            ("Held.swap(next)", "Held/swap"),
            ("Written.swap(next)", "Written/swap"),
            ("Dynamic.swap(next)", "Dynamic/swap"),
            ("Dynamic.make()", "Dynamic/make"),
            ("Lexical.swap(next)", "Lexical/swap"),
        ]
    );
}

#[test]
fn package_javascript_class_alias_writes_withhold_the_replaced_method() {
    let source = r"
export class Aliased { static make() { return 1; } }
const Alias = Aliased;
export class Direct { static make() { return 2; } }
export class Scoped { static make() { return 3; } }
export class Other { static make() { return 4; } }
export class Unchanged { static make() { return 5; } }
const Exposed = Scoped;
let Former = Unchanged;
Former = Other;
export function throughAlias(next) { Alias.make = next; return Aliased.make(); }
export function throughDirect(next) { Direct.make = next; return Direct.make(); }
export function nested(next) { const Exposed = Other; Exposed.make = next; return Scoped.make(); }
export function reassigned(next) { Former.make = next; return Unchanged.make(); }
";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .map(|edge| {
            let span = edge.occurrence.as_ref().expect("written call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            let owner = edge.from.as_str().rsplit("/beacon/").next().expect("owner");
            let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
            (start, &source[start..end], owner, target)
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let calls = calls
        .into_iter()
        .map(|(_, text, owner, target)| (text, owner, target))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            ("Scoped.make()", "nested", "Scoped/make"),
            ("Unchanged.make()", "reassigned", "Unchanged/make"),
        ]
    );
}

struct GraphFixture {
    analysis: PackageAnalysis,
    owner: SymbolOwner,
    origin: ContributionOrigin,
    language: Language,
    anchor_language: Option<Language>,
}

impl GraphFixture {
    fn new() -> Self {
        let package = fixture::identity();
        Self {
            analysis: fixture::package_result(
                rift_syntax::ShippedLanguage::Rust,
                vec![("src/lib.rs", SOURCE)],
                None,
            )
            .expect("captured physical source"),
            owner: package.owner().expect("canonical package owner"),
            origin: fixture::origin(&package),
            language: Language::from_identity_segment("rust").expect("language"),
            anchor_language: None,
        }
    }

    fn identity(&self, name: &str) -> rift_core::SymbolId {
        let identity = rift_protocol::identity::SymbolIdentity::new(
            self.owner.clone(),
            self.anchor_language
                .as_ref()
                .unwrap_or(&self.language)
                .clone(),
            vec!["beacon".to_owned(), name.to_owned()],
        )
        .expect("current logical identity");
        rift_core::SymbolId::new(identity.wire_identity()).expect("symbol address")
    }

    fn reference(&self, name: &str) -> ContributionReference {
        ContributionReference::new(
            Self::provider(),
            ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
        )
    }

    fn provider() -> ProviderId {
        ProviderId::new("publication.fixture").expect("provider")
    }

    fn contribution(&self, name: &str, edges: Vec<ContributionEdge>) -> Contribution {
        Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            PortableSymbolFacts::new(
                self.language.clone(),
                name,
                format!("beacon::{name}"),
                ExactKind::try_from("function".to_owned()).expect("kind"),
            ),
            self.origin.clone(),
        )
        .identity_anchor(self.identity(name))
        .edges(edges)
        .build()
        .expect("source-less logical contribution")
    }

    fn graph(&self, edges: Vec<ContributionEdge>) -> WorkspaceSemantics {
        Self::graph_with_contributions(vec![
            self.contribution("start", edges),
            self.contribution("finish", Vec::new()),
        ])
    }

    fn graph_with_contributions(contributions: Vec<Contribution>) -> WorkspaceSemantics {
        let limits = PublicationLimits::new(1, 2, 2).expect("graph bounds");
        let publication = ProviderPublication::new(
            Self::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            contributions,
            limits,
        )
        .expect("captured contributions");
        let publications = std::sync::Arc::new(
            PublicationSet::empty(limits)
                .replaced(publication)
                .expect("bounded publication set"),
        );
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            &publications,
            None,
        )
        .expect("normalized graph");
        WorkspaceSemantics::from_graph(graph, Self::provider(), 16)
    }

    fn named_contribution(
        &self,
        name: &str,
        edges: Vec<ContributionEdge>,
        established: bool,
    ) -> Result<Contribution, RiftError> {
        use rift_protocol::read::{Documentation, DocumentationFormat, Extensions, Signature};

        let signatures = ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
            .into_iter()
            .map(|display| Signature {
                display: display.to_owned(),
                links: Vec::new(),
                language: self.language.clone(),
                receiver: None,
                parameters: Vec::new(),
                returns: Vec::new(),
                type_parameters: Vec::new(),
                throws: Vec::new(),
                effects: Vec::new(),
                extensions: Extensions::default(),
            })
            .collect();
        let facts = PortableSymbolFacts::new(
            self.language.clone(),
            name,
            "beacon::wide",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        )
        .signatures(signatures)
        .documentation(vec![Documentation {
            format: DocumentationFormat::Markdown,
            text: "Both callable forms remain in the captured object.".to_owned(),
        }]);
        let mut builder = Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity("wide").as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            facts,
            self.origin.clone(),
        )
        .edges(edges);
        if established {
            builder = builder.identity_anchor(self.identity("wide"));
        }
        builder.build()
    }

    fn occurrence(&self, start: usize, end: usize) -> DeclarationBinding {
        DeclarationBinding::new(
            self.analysis.files()[0].placement.unit().clone(),
            SourceRange::new(
                u64::try_from(start).expect("source start"),
                u64::try_from(end).expect("source end"),
            )
            .expect("nonempty source range"),
        )
    }

    fn edge(&self, occurrence: DeclarationBinding) -> ContributionEdge {
        ContributionEdge::new(rift_core::EdgeKind::Reference, self.reference("finish"))
            .with_derivation(EdgeDerivation::Syntax)
            .with_occurrence(occurrence)
    }

    fn publication(
        &self,
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<PackagePublication, RiftError> {
        let sources = self
            .analysis
            .files()
            .iter()
            .map(|held| {
                crate::PackageSource::new(held.file.path(), held.file.source())
                    .with_source_unit(held.placement.unit(), held.placement.origin())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let input =
            ExactPackageInput::new(&self.owner, &self.language, &self.origin, &sources, limits)?;
        publish(&input, self.analysis.files(), semantics, false).map(|(publication, _)| publication)
    }
}

#[test]
fn artifact_name_bytes_and_public_name_characters_keep_distinct_bounds() {
    let fixture = GraphFixture::new();
    for (name, public) in [
        ("n".repeat(4096), true),
        ("n".repeat(4097), false),
        ("é".repeat(4096), true),
        ("n".repeat(8192), false),
    ] {
        assert!(rift_core::is_portable_name(&name));
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, Vec::new(), true)
                .expect("provider name within byte bound"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("full artifact name retained");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("original normalized object");
        assert_eq!(object.name_is_public(), public);
        assert!(object.id.is_some());
        assert_eq!(object.signatures.len(), 2);
        assert_eq!(
            publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, field, bound, .. }
                if field == "name" && *bound == 4096)),
            !public
        );
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| coverage.identity_complete == public)
        );
        let schema: serde_json::Value =
            serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
                .expect("artifact schema");
        let validator = jsonschema::validator_for(&schema).expect("artifact schema compiles");
        assert!(validator.is_valid(&serde_json::to_value(&publication).expect("artifact")));
    }
    for name in ["n".repeat(8193), "é".repeat(4097)] {
        assert!(!rift_core::is_portable_name(&name));
        assert!(fixture.named_contribution(&name, Vec::new(), true).is_err());
    }
}

#[test]
fn source_less_wide_objects_roundtrip_all_facts_and_edge_occurrences() {
    let fixture = GraphFixture::new();
    let name = "n".repeat(4097);
    for established in [true, false] {
        let edges = if established {
            SOURCE
                .match_indices("finish();")
                .map(|(start, _)| fixture.edge(fixture.occurrence(start, start + "finish()".len())))
                .collect()
        } else {
            Vec::new()
        };
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, edges, established)
                .expect("full source-less facts"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("source-less artifact projection");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("full source-less object");
        assert_eq!(object.id.is_some(), established);
        assert!(!object.name_is_public());
        assert_eq!(
            object
                .signatures
                .iter()
                .map(|signature| signature.display.as_str())
                .collect::<Vec<_>>(),
            ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
        );
        assert_eq!(
            object.documentation[0].text,
            "Both callable forms remain in the captured object."
        );
        assert!(publication.declarations.is_empty());
        assert!(
            !publication
                .documents
                .iter()
                .any(|document| document.name == name)
        );
        assert_eq!(publication.units[0].source, SOURCE);
        assert!(publication.units[0].source_complete);
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| !coverage.identity_complete)
        );
        assert!(publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, qualified_name, field, bound }
                if qualified_name == "beacon::wide" && field == "name" && *bound == 4096)));
        if !established {
            assert!(publication.warnings.iter().any(|warning| matches!(warning,
                PackageAnalysisWarning::IdentityUnresolved { unit: None, range: None, qualified_name }
                    if qualified_name == "beacon::wide")));
        }
        let wire = serde_json::to_value(&publication).expect("full artifact serialization");
        let restored: PackagePublication =
            serde_json::from_value(wire).expect("full artifact restore");
        assert_eq!(restored, publication);
        assert_eq!(
            semantics.graph().edges().len(),
            if established { 2 } else { 0 }
        );
        if established {
            assert_eq!(restored.edges.len(), 2);
            let starts = restored
                .edges
                .iter()
                .map(|edge| {
                    let occurrence = edge.occurrence.as_ref().expect("actual source occurrence");
                    assert_eq!(occurrence.unit, restored.units[0].unit);
                    let start = usize::try_from(occurrence.range.start).expect("source start");
                    let end = usize::try_from(occurrence.range.end).expect("source end");
                    assert_eq!(&SOURCE[start..end], "finish()");
                    start
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(starts.len(), 2);
        }
    }
}

#[test]
fn public_name_omission_retains_original_source_binding_and_artifact_object() {
    let name = "n".repeat(4097);
    let source = format!("pub fn {name}() {{}}\n");
    let analysis = fixture::package_result(
        rift_syntax::ShippedLanguage::Rust,
        vec![("src/lib.rs", &source)],
        None,
    )
    .expect("captured Cargo root and source");
    let publication = analysis.publication();
    let object = publication
        .objects
        .iter()
        .find(|object| object.name == name)
        .expect("full artifact object");
    let id = object.id.as_ref().expect("established canonical identity");
    assert!(!object.name_is_public());
    let bindings = publication
        .declarations
        .iter()
        .filter(|binding| &binding.symbol == id)
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 1);
    let binding = bindings[0];
    assert!(binding.public);
    assert!(binding.source_complete);
    let start = usize::try_from(binding.range.start).expect("original start");
    let end = usize::try_from(binding.range.end).expect("original end");
    assert_eq!(binding.source, source[start..end]);
    assert_eq!(publication.units[0].source, source);
    assert!(
        !publication
            .documents
            .iter()
            .any(|document| document.kind == PackageDocumentKind::Symbol)
    );
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::ObjectUnavailable { unit: Some(unit), range: Some(range), qualified_name, field, bound }
            if unit == &binding.unit && range == &binding.range && qualified_name == &name
                && field == "name" && *bound == 4096)));
}

#[test]
fn source_less_objects_keep_logical_runtime_origin() {
    let mut fixture = GraphFixture::new();
    let runtime = rift_protocol::read::RuntimeIdentity {
        runtime: "rust".to_owned(),
        version: "1.98.1".to_owned(),
    };
    fixture.owner = runtime.owner().expect("logical runtime owner");
    fixture.origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime.clone()),
        }),
        rift_core::SourceKind::Authored,
    )
    .expect("runtime origin");
    let semantics = fixture.graph(Vec::new());
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("source-less object projection");
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.declarations.is_empty());
    for object in &publication.objects {
        let identity = rift_protocol::identity::SymbolIdentity::parse(
            object.id.as_ref().expect("established identity").as_str(),
        )
        .expect("canonical runtime identity");
        assert_eq!(identity.owner(), &fixture.owner);
        assert_eq!(object.origin.runtime.as_ref(), Some(&runtime));
        assert!(object.origin.package.is_none());
    }
    assert!(
        publication
            .documents
            .iter()
            .all(|document| { document.kind != rift_protocol::index::PackageDocumentKind::Symbol })
    );
    assert_eq!(semantics.graph().records().len(), 2);
}

#[test]
fn captured_records_assemble_source_less_facts_without_assigning_unknown_identity() {
    let fixture = GraphFixture::new();
    for established in [true, false] {
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution("wide", Vec::new(), established)
                .expect("source-less contribution"),
        ]);
        let record = &semantics.graph().records()[0];
        for key in record.contributions() {
            assert!(
                semantics
                    .graph()
                    .contribution(key)
                    .expect("captured contribution")
                    .source()
                    .is_none()
            );
        }
        let object = semantics
            .assembled_record(record)
            .expect("source-less object assembly");
        assert_eq!(object.identity().is_some(), established);
        if established {
            assert_eq!(object.identity(), Some(&fixture.identity("wide")));
        }
        let facts = object.facts().expect("retained facts");
        assert_eq!(facts.name(), "wide");
        assert_eq!(facts.signatures_slice().len(), 2);
        assert_eq!(facts.documentation_blocks().len(), 1);
    }
}

#[test]
fn foreign_logical_owner_cannot_receive_publication_origin() {
    let fixture = GraphFixture::new();
    let mut foreign = GraphFixture::new();
    foreign.owner = SymbolOwner::Local;
    let local = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&local, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign owner retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(local.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    let SymbolOwner::Package { registry, .. } = &mut foreign.owner else {
        panic!("fixture requires a package owner");
    };
    *registry = "private.example".to_owned();
    foreign
        .owner
        .validate()
        .expect("canonical foreign registry");
    let private = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&private, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign registry retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(private.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    foreign.anchor_language = Some(Language::from_identity_segment("python").expect("language"));
    let invalid = Contribution::builder(
        ContributionKey::new(
            GraphFixture::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            ProviderSymbolId::for_symbol(foreign.identity("start").as_str()).expect("provider key"),
        ),
        SourceApplicability::Independent,
        PortableSymbolFacts::new(
            foreign.language.clone(),
            "start",
            "beacon::start",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        ),
        foreign.origin.clone(),
    )
    .identity_anchor(foreign.identity("start"))
    .build();
    assert!(invalid.is_err());
}

fn assert_foreign_facts_retained(publication: &PackagePublication) {
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.objects.iter().all(|object| object.id.is_none()));
    assert_eq!(
        publication
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["start", "finish"])
    );
    assert!(publication.declarations.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(
        publication
            .warnings
            .iter()
            .filter(|warning| matches!(
                warning,
                PackageAnalysisWarning::IdentityUnresolved {
                    unit: None,
                    range: None,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        publication
            .warnings
            .iter()
            .all(PackageAnalysisWarning::is_valid)
    );
}

#[test]
fn portable_edges_keep_two_original_call_sites_without_syntax_elements() {
    let fixture = GraphFixture::new();
    let first = SOURCE.find("finish();").expect("first call");
    let second = SOURCE.rfind("finish();").expect("second call");
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(first, first + "finish()".len())),
        fixture.edge(fixture.occurrence(second, second + "finish()".len())),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_retained_source_bytes(8, 1024)
                .expect("bounded retained excerpts"),
        )
        .expect("portable occurrence projection");
    assert_eq!(publication.edges.len(), 2);
    assert!(!publication.units[0].source_complete);
    let starts = publication
        .edges
        .iter()
        .map(|edge| {
            assert_eq!(edge.from.0, fixture.identity("start").as_str());
            assert_eq!(edge.to.0, fixture.identity("finish").as_str());
            assert_eq!(edge.derivation, EdgeDerivation::Syntax);
            assert_eq!(edge.kinds, [EdgeKind::References]);
            assert!(edge.confidence.is_none());
            let occurrence = edge.occurrence.as_ref().expect("original call site");
            assert_eq!(occurrence.unit, publication.units[0].unit);
            let start = usize::try_from(occurrence.range.start).expect("source start");
            let end = usize::try_from(occurrence.range.end).expect("source end");
            assert_eq!(&SOURCE[start..end], "finish()");
            start
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(starts, BTreeSet::from([first, second]));
    assert_eq!(semantics.graph().edges().len(), 2);
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.edges.is_empty())
    );
}

#[test]
fn absent_derivation_keeps_graph_fact_without_portable_edge() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![ContributionEdge::new(
        rift_core::EdgeKind::Reference,
        fixture.reference("finish"),
    )]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unresolved edge projection");
    assert_eq!(semantics.graph().edges().len(), 1);
    assert!(publication.edges.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.edges.is_empty())
    );
}

#[test]
fn invalid_occurrence_refuses_original_byte_range_and_unknown_unit() {
    let fixture = GraphFixture::new();
    for (start, end) in [(0, SOURCE.len() + 1), (4, 5)] {
        let semantics = fixture.graph(vec![fixture.edge(fixture.occurrence(start, end))]);
        assert!(
            fixture
                .publication(&semantics, ExactPackageLimits::new(1, 1024))
                .is_err()
        );
        assert_eq!(semantics.graph().edges().len(), 1);
    }
    let unit = rift_core::SourceUnitId::for_owner(fixture.owner.clone(), "missing.rs")
        .expect("canonical unit outside selected inventory");
    let occurrence =
        DeclarationBinding::new(unit, SourceRange::new(0, 1).expect("nonempty source range"));
    let semantics = fixture.graph(vec![fixture.edge(occurrence)]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unavailable occurrence projection");
    assert!(publication.edges.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(publication.units[0].source, SOURCE);
    assert_eq!(semantics.graph().edges().len(), 1);
}

#[test]
fn edge_bound_keeps_captured_graph_immutable() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(0, 1)),
        fixture.edge(fixture.occurrence(1, 2)),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_edges(1)
                .expect("portable edge bound"),
        )
        .expect("bounded portable edges");
    assert_eq!(publication.edges.len(), 1);
    assert_eq!(semantics.graph().edges().len(), 2);
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::PublicationTruncated { collection, bound }
            if collection == "edges" && *bound == 1)));
}

#[test]
fn four_byte_names_keep_full_artifact_facts_and_neighbor_publication() {
    for characters in [3000, 4097] {
        let name = "𐐀".repeat(characters);
        let source = format!(
            "/// Original callable documentation.\npub fn {name}(value: u8) -> u8 {{ value }}\npub fn finish() {{}}\n"
        );
        let analysis = fixture::package_result(
            rift_syntax::ShippedLanguage::Rust,
            vec![("src/lib.rs", &source)],
            None,
        )
        .expect("captured full source and package metadata");
        let publication = analysis.publication();
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("full omitted object");
        assert!(object.id.is_none());
        assert_eq!(object.name_is_public(), characters <= 4096);
        assert_eq!(object.signatures.len(), 1);
        assert!(object.signatures[0].display.contains(&name));
        assert_eq!(
            object.documentation[0].text,
            "Original callable documentation."
        );
        assert!(
            publication
                .objects
                .iter()
                .any(|object| object.name == "finish" && object.id.is_some())
        );
        assert!(
            publication
                .documents
                .iter()
                .any(|document| document.kind == PackageDocumentKind::Symbol
                    && document.name == "finish")
        );
        assert!(
            !publication
                .documents
                .iter()
                .any(|document| document.name == name)
        );
        assert_eq!(publication.units[0].source, source);
        assert!(publication.units[0].source_complete);
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| !coverage.identity_complete)
        );
        assert!(publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::IdentityUnresolved { unit: Some(unit), range: Some(range), qualified_name }
            if unit == &publication.units[0].unit && qualified_name == &name
                && source[usize::try_from(range.start).expect("item start")..usize::try_from(range.end).expect("item end")].contains(&name))));
        assert_eq!(
            publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { field, bound, qualified_name, .. }
            if field == "name" && *bound == 4096 && qualified_name == &name)),
            characters > 4096
        );
        let wire = serde_json::to_value(publication).expect("full artifact encoding");
        let schema: serde_json::Value =
            serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
                .expect("artifact schema");
        assert!(
            jsonschema::validator_for(&schema)
                .expect("schema validator")
                .is_valid(&wire)
        );
        let restored: PackagePublication =
            serde_json::from_value(wire).expect("full artifact restore");
        assert_eq!(&restored, publication);
    }
}
