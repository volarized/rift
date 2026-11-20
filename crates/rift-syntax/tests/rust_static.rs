//! Rust declarations and static syntax evidence.

use rift_core::ProjectPath;
use rift_syntax::{RustSyntaxProvider, SyntaxLimits, SyntaxProvider, SyntaxSource};

#[test]
fn test_rust_reference_and_slice_impls_keep_authored_owner_ranges() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "impl<'a> Count for &'a Item { fn read(&self) {} } impl Count for &mut Item { fn read(&self) {} } impl Count for [u8] { fn read(&self) {} } impl Count for &[u8] { fn read(&self) {} }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let implementations = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == "impl_item")
        .collect::<Vec<_>>();
    assert_eq!(
        implementations
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["&Item", "&mut Item", "[u8]", "&[u8]"]
    );
    let facts = document.facts().static_facts().expect("static facts");
    assert_eq!(
        facts
            .inherent_impls()
            .iter()
            .map(
                |implementation| &source[usize::try_from(implementation.type_path.start)
                    .expect("start")
                    ..usize::try_from(implementation.type_path.end).expect("end")]
            )
            .collect::<Vec<_>>(),
        ["&'a Item", "&mut Item", "[u8]", "&[u8]"]
    );
    for implementation in implementations {
        assert!(document.symbols().iter().any(|member| member.name == "read"
            && member.container.as_deref() == Some(implementation.name.as_str())));
    }
}

#[test]
fn test_rust_grouped_imports_keep_each_binding() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "use crate::jobs::{Task, leaf as run};";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let facts = document.facts().static_facts().expect("static facts");
    assert_eq!(facts.bindings().len(), 2);
    assert!(facts.unsupported_bindings().is_empty());
}

#[test]
fn test_rust_generic_impl_keeps_its_named_type() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "struct Task<T> { value: T } impl<T> Task<T> { fn new() {} }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let facts = document.facts().static_facts().expect("static facts");
    assert_eq!(facts.inherent_impls().len(), 1);
}

#[test]
fn test_rust_impl_is_a_facet_of_its_named_type() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "struct Task; impl Task { fn run(&self) {} }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let implementation = document
        .symbols()
        .iter()
        .find(|symbol| symbol.kind == "impl_item")
        .expect("impl declaration");
    assert_eq!(implementation.name, "Task");
    assert_eq!(implementation.qualified_name, "Task");
    assert!(implementation.body_range.is_some());
}

#[test]
fn test_rust_cfg_attr_without_cfg_keeps_items_unconditional() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "#![cfg_attr(docsrs, feature(doc_cfg))]\n#![cfg_attr(not(feature = \"std\"), no_std)]\nfn leaf() {}";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    assert!(
        document
            .facts()
            .static_facts()
            .expect("static facts")
            .conditional_items()
            .is_empty()
    );
}

#[test]
fn test_rust_named_closure_keeps_a_callable_declaration() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "fn run() { let next = || 1; next(); }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let closure = document
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "next")
        .expect("closure declaration");
    assert_eq!(closure.kind, "let_declaration");
    assert_eq!(closure.container.as_deref(), Some("run"));
}

#[test]
fn test_rust_wildcard_closure_does_not_declare_a_binding() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: "fn run() { let _ = || 1; }",
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    assert!(
        !document
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "let_declaration")
    );
}

#[test]
fn test_rust_use_paths_stay_inside_their_recorded_scope() {
    use rift_syntax::{ByteRange, StaticFacts, StaticFactsParts, StaticRustUse};
    let parts = StaticFactsParts {
        rust_uses: vec![StaticRustUse {
            range: ByteRange { start: 2, end: 3 },
            path: vec![ByteRange { start: 0, end: 1 }],
            scope: ByteRange { start: 2, end: 3 },
            glob: false,
            visibility: None,
        }],
        ..StaticFactsParts::default()
    };
    assert!(StaticFacts::from_parts("a b", SyntaxLimits::default(), parts).is_err());
}

#[test]
fn test_rust_use_path_components_share_the_syntax_bound() {
    use rift_syntax::{ByteRange, StaticFacts, StaticFactsParts, StaticRustUse};
    let range = ByteRange { start: 0, end: 1 };
    let parts = StaticFactsParts {
        rust_uses: vec![
            StaticRustUse {
                range,
                path: vec![range; 3],
                scope: range,
                glob: false,
                visibility: None,
            };
            3
        ],
        ..StaticFactsParts::default()
    };
    let limits = SyntaxLimits::new(10, 8, 10).expect("limits");
    assert!(StaticFacts::from_parts("a", limits, parts).is_err());
}

#[test]
fn test_rust_grouped_imports_refuse_before_copying_past_the_bound() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let names = (0..40)
        .map(|index| format!("t{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let text = format!(
        "use crate::{{{}{}{}}};",
        "a::{".repeat(30),
        names,
        "}".repeat(30)
    );
    let source = SyntaxSource {
        path: &path,
        text: &text,
    };
    let provider = RustSyntaxProvider::default();
    assert!(provider.analyze(source, SyntaxLimits::default()).is_ok());
    let limits = SyntaxLimits::new(10_000, 1_024, 128).expect("limits");
    let error = provider
        .analyze(source, limits)
        .expect_err("copied paths exceed bound");
    assert_eq!(error.slug().as_str(), "rift.syntax.facts_count_exceeded");
}

#[test]
fn test_rust_trait_default_captures_its_receiver_owner() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source =
        "trait Count { fn remaining(&self) -> usize; fn get(&self) -> usize { self.remaining() } }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let facts = document.facts().static_facts().expect("static facts");
    assert!(facts.bindings().iter().any(|binding| {
        matches!(binding.kind, rift_syntax::StaticBindingKind::RustReceiver { type_path } if &source[usize::try_from(type_path.start).expect("offset")..usize::try_from(type_path.end).expect("offset")] == "Count")
    }));
}

#[test]
fn test_rust_inline_and_where_bounds_retain_authored_paths() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source =
        "fn inline<T: Count>(item: &T) {} fn bounded<T>(item: &T) where T: crate::Count {}";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let facts = document.facts().static_facts().expect("static facts");
    let bounds = facts
        .declared_types()
        .iter()
        .map(|&(name, bound)| {
            let text = |range: rift_syntax::ByteRange| {
                &source[usize::try_from(range.start).expect("offset")
                    ..usize::try_from(range.end).expect("offset")]
            };
            (text(name), text(bound))
        })
        .collect::<Vec<_>>();
    assert_eq!(bounds, [("T", "Count"), ("T", "crate::Count")]);
}

#[test]
fn test_rust_conditional_items_keep_their_authored_attribute_ranges() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "#[cfg(feature=\"std\")]\nfn leaf() {}";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let conditions = document
        .facts()
        .static_facts()
        .expect("static facts")
        .conditional_items();
    assert_eq!(conditions.len(), 1);
    let condition = conditions[0];
    let text = |range: rift_syntax::ByteRange| {
        &source[usize::try_from(range.start).expect("offset")
            ..usize::try_from(range.end).expect("offset")]
    };
    assert_eq!(text(condition.attribute), "#[cfg(feature=\"std\")]");
    assert_eq!(text(condition.range), "fn leaf() {}");
}

#[test]
fn test_rust_local_return_facts_keep_authored_initializer_and_return_type() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "struct Item; impl Item { fn copy(&self) -> Self { Self } fn run(&self) { let copy = self.copy(); } }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let facts = document.facts().static_facts().expect("static facts");
    let text = |range: rift_syntax::ByteRange| {
        &source[usize::try_from(range.start).expect("offset")
            ..usize::try_from(range.end).expect("offset")]
    };
    assert!(
        facts
            .declared_types()
            .iter()
            .any(|&(name, result)| text(name) == "copy" && text(result) == "Self")
    );
    assert!(facts.bindings().iter().any(|binding| matches!(binding.kind, rift_syntax::StaticBindingKind::RustValue { initializer } if text(binding.name) == "copy" && text(initializer) == "self.copy()")));
}

#[test]
fn test_rust_macro_token_errors_keep_authored_declarations_and_ranges() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "macro_rules! marker { (~) => {}; } fn leaf() {} fn run() { leaf(); }";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let marker = document
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "marker")
        .expect("macro");
    assert_eq!(
        &source[usize::try_from(marker.item_range.start).expect("start")
            ..usize::try_from(marker.item_range.end).expect("end")],
        "macro_rules! marker { (~) => {}; }"
    );
    let invalid = RustSyntaxProvider::default()
        .invalid_declarations(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
            &[marker.item_range],
        )
        .expect("validation");
    assert!(
        invalid.is_empty(),
        "opaque body errors do not invalidate the macro header"
    );
    let facts = document.facts().static_facts().expect("static facts");
    assert_eq!(facts.calls().len(), 1);
}

#[test]
fn test_rust_reexport_imports_keep_authored_visibility_ranges() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let source = "pub(crate) use crate::Item; use crate::Other;";
    let document = RustSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            SyntaxLimits::default(),
        )
        .expect("syntax");
    let uses = document
        .facts()
        .static_facts()
        .expect("static facts")
        .rust_uses();
    let visibility = uses[0].visibility.expect("restricted visibility");
    assert_eq!(
        &source[usize::try_from(visibility.start).expect("start")
            ..usize::try_from(visibility.end).expect("end")],
        "pub(crate)"
    );
    assert!(uses[1].visibility.is_none());
}
