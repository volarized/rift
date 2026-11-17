//! Declarations the ECMAScript rules emit beyond functions, classes, and
//! plain variables: abstract classes, class fields, enum members,
//! destructured bindings, and declarations nested in callables.

use rift_protocol::read::SymbolFacet;

use super::*;
use crate::javascript::JavaScriptSyntaxProvider;
use crate::provider::SyntaxProvider;
use crate::typescript::{TypeScriptDialect, TypeScriptSyntaxProvider};

fn analyze(provider: &dyn SyntaxProvider, path: &str, text: &str) -> SyntaxDocument {
    let path = rift_core::ProjectPath::new(path).expect("fixture path");
    let document = provider
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("fixture source");
    assert!(!document.has_errors());
    document
}

fn typescript(text: &str) -> SyntaxDocument {
    analyze(
        &TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript),
        "src/main.ts",
        text,
    )
}

fn javascript(text: &str) -> SyntaxDocument {
    analyze(&JavaScriptSyntaxProvider::default(), "src/main.js", text)
}

/// Each declaration's qualified name, container, and kind word.
fn declared(document: &SyntaxDocument) -> Vec<(&str, Option<&str>, &str)> {
    document
        .symbols()
        .iter()
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.container.as_deref(),
                symbol.kind,
            )
        })
        .collect()
}

fn symbol<'document>(
    document: &'document SyntaxDocument,
    qualified: &str,
) -> &'document crate::SyntaxSymbol {
    document
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == qualified)
        .unwrap_or_else(|| panic!("fixture declares {qualified}: {:?}", declared(document)))
}

fn spelled(text: &str, range: ByteRange) -> &str {
    let start = usize::try_from(range.start).expect("fixture start");
    let end = usize::try_from(range.end).expect("fixture end");
    &text[start..end]
}

/// An `abstract class` declares a class, so its members keep it as their
/// container, and an abstract method signature declares a method.
#[test]
fn test_abstract_class_declares_a_class_containing_its_members() {
    let document = typescript(
        "export abstract class Shape {\n  abstract area(): number;\n  draw(): void {}\n}\n",
    );
    assert_eq!(
        declared(&document),
        [
            ("Shape", None, "class"),
            ("Shape.area", Some("Shape"), "method"),
            ("Shape.draw", Some("Shape"), "method"),
        ]
    );
    let shape = symbol(&document, "Shape");
    assert!(shape.facets.contains(&SymbolFacet::Abstract));
    assert!(shape.facets.contains(&SymbolFacet::Public));
    let area = symbol(&document, "Shape.area");
    assert!(area.facets.contains(&SymbolFacet::Abstract));
    assert!(area.facets.contains(&SymbolFacet::Callable));
    assert_eq!(
        area.signatures
            .first()
            .map(|signature| signature.display.as_str()),
        Some("abstract area(): number")
    );
}

/// Class fields declare properties of their class in both grammars, with
/// their `JSDoc`, accessibility, and private names; a constructor parameter
/// property declares a property of the class, not of the constructor.
#[test]
fn test_class_fields_and_parameter_properties_declare_class_properties() {
    let text = "class Shape {\n  /** Side count. */\n  sides: number = 0;\n  \
                private static count = 1;\n  #secret = 2;\n  \
                constructor(public readonly name: string, plain: number) {}\n}\n";
    let document = typescript(text);
    assert_eq!(
        declared(&document),
        [
            ("Shape", None, "class"),
            ("Shape.sides", Some("Shape"), "property"),
            ("Shape.count", Some("Shape"), "property"),
            ("Shape.#secret", Some("Shape"), "property"),
            ("Shape.constructor", Some("Shape"), "method"),
            ("Shape.name", Some("Shape"), "property"),
        ]
    );
    let sides = symbol(&document, "Shape.sides");
    assert_eq!(sides.facets, [SymbolFacet::Value, SymbolFacet::Member]);
    assert_eq!(
        sides.documentation.first().map(|doc| doc.text.as_str()),
        Some("Side count.")
    );
    assert_eq!(
        spelled(text, sides.range),
        "/** Side count. */\n  sides: number = 0"
    );
    assert_eq!(
        symbol(&document, "Shape.count").visibility.as_deref(),
        Some("private")
    );
    assert_eq!(
        symbol(&document, "Shape.name").visibility.as_deref(),
        Some("public")
    );

    let document =
        javascript("class Widget {\n  count = 0;\n  static made = 1;\n  #secret = 2;\n}\n");
    assert_eq!(
        declared(&document),
        [
            ("Widget", None, "class"),
            ("Widget.count", Some("Widget"), "property"),
            ("Widget.made", Some("Widget"), "property"),
            ("Widget.#secret", Some("Widget"), "property"),
        ]
    );
    let count = symbol(&document, "Widget.count");
    assert_eq!(
        count.name_range.map(|range| spelled(
            "class Widget {\n  count = 0;\n  static made = 1;\n  #secret = 2;\n}\n",
            range
        )),
        Some("count")
    );
}

/// Every member of an enum declares an `enum_member` of the enum, whether
/// it assigns a value or not.
#[test]
fn test_enum_members_declare_under_their_enum() {
    let text = "export enum Kind { A = 1, B, C = \"c\" }\n";
    let document = typescript(text);
    assert_eq!(
        declared(&document),
        [
            ("Kind", None, "enum"),
            ("Kind.A", Some("Kind"), "enum_member"),
            ("Kind.B", Some("Kind"), "enum_member"),
            ("Kind.C", Some("Kind"), "enum_member"),
        ]
    );
    let member = symbol(&document, "Kind.B");
    assert_eq!(member.facets, [SymbolFacet::Variant]);
    assert_eq!(
        member.name_range.map(|range| spelled(text, range)),
        Some("B")
    );
    assert_eq!(
        spelled(text, symbol(&document, "Kind.C").range),
        "C = \"c\""
    );
}

/// A destructuring declarator declares every name its pattern binds, each
/// at its own range, and an export of the statement marks each name.
#[test]
fn test_destructured_bindings_declare_one_variable_per_bound_name() {
    let text = "export const { major, minor: mi, ...rest } = version;\n\
                const [first, [second], third = fallback] = list;\n\
                const { a = b, c: [d] } = other;\n";
    let document = javascript(text);
    let names = document
        .symbols()
        .iter()
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.kind,
                spelled(text, symbol.range),
                symbol.facets.contains(&SymbolFacet::Public),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            ("major", "variable", "major", true),
            ("mi", "variable", "mi", true),
            ("rest", "variable", "rest", true),
            ("first", "variable", "first", false),
            ("second", "variable", "second", false),
            ("third", "variable", "third", false),
            ("a", "variable", "a", false),
            ("d", "variable", "d", false),
        ]
    );
}

/// Declarations inside a function, a method, or a variable's value qualify
/// under it, so two nested names in different parents stay apart and no
/// local is filed at module scope.
#[test]
fn test_nested_declarations_qualify_under_their_enclosing_declaration() {
    let text = "function inner() { const local = () => 1; function nested() {} }\n\
                function other() { const local = 2; }\n\
                class Widget { go() { const step = 1; } }\n\
                const obj = { method() { return 1; }, prop: 2 };\n\
                const { picked } = obj;\n";
    let document = javascript(text);
    assert_eq!(
        declared(&document),
        [
            ("inner", None, "function"),
            ("inner.local", Some("inner"), "variable"),
            ("inner.nested", Some("inner"), "function"),
            ("other", None, "function"),
            ("other.local", Some("other"), "variable"),
            ("Widget", None, "class"),
            ("Widget.go", Some("Widget"), "method"),
            ("Widget.go.step", Some("Widget.go"), "variable"),
            ("obj", None, "variable"),
            ("obj.method", Some("obj"), "method"),
            ("picked", None, "variable"),
        ]
    );
}

/// The anonymous class a module exports by default declares a class named
/// `default`, the name the module exports it under, and its members keep it
/// as their container.
#[test]
fn test_anonymous_default_class_contains_its_members() {
    let document = javascript("export default class { anon() {} }\n");
    assert_eq!(
        declared(&document),
        [
            ("default", None, "class"),
            ("default.anon", Some("default"), "method"),
        ]
    );
    assert!(
        symbol(&document, "default")
            .facets
            .contains(&SymbolFacet::Public)
    );
}

/// A variable holding an arrow function or a function expression can be
/// invoked, so it carries the `callable` category beside `value`.
#[test]
fn test_function_valued_variables_carry_the_callable_category() {
    let document = javascript(
        "export const make = (n) => n;\nconst legacy = function () {};\nconst plain = 1;\n",
    );
    let facets = document
        .symbols()
        .iter()
        .map(|symbol| (symbol.name.as_str(), symbol.facets.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        facets,
        [
            (
                "make",
                vec![
                    SymbolFacet::Value,
                    SymbolFacet::Callable,
                    SymbolFacet::Public
                ]
            ),
            ("legacy", vec![SymbolFacet::Value, SymbolFacet::Callable]),
            ("plain", vec![SymbolFacet::Value]),
        ]
    );
}
