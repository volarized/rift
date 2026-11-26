//! Declarations the ECMAScript rules emit beyond functions, classes, and
//! plain variables: abstract classes, class fields, enum members,
//! destructured bindings, and declarations nested in callables.

use rift_protocol::read::SymbolKind;

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

#[test]
fn test_commonjs_local_object_members_distinguish_receiver_declarations_from_body_aliases() {
    for (source, method, alias) in [
        (
            "var res = {}; res.clear = function() { var res = this; return res; };",
            "res.clear",
            "res.clear.res",
        ),
        (
            "function View() {} View.prototype.render = function() { var View = this; return View; };",
            "View.render",
            "View.render.View",
        ),
    ] {
        for document in [javascript(source), typescript(source)] {
            assert_eq!(symbol(&document, method).kind, "method");
            assert_eq!(symbol(&document, alias).kind, "variable");
        }
    }
}

#[test]
fn test_commonjs_local_object_members_keep_attached_documentation() {
    for (prefix, assignment, name) in [
        ("var object = {};", "object.run", "object.run"),
        ("function View() {}", "View.prototype.run", "View.run"),
    ] {
        for gap in ["\n", "\n\n"] {
            let source = format!("{prefix}\n/** Runs. */{gap}{assignment} = function() {{}};");
            for document in [javascript(&source), typescript(&source)] {
                let method = symbol(&document, name);
                assert!(spelled(&source, method.item_range).starts_with(assignment));
                if gap == "\n" {
                    assert_eq!(method.documentation[0].text, "Runs.");
                    assert!(spelled(&source, method.range).starts_with("/** Runs. */"));
                    assert_eq!(
                        spelled(&source, method.documentation_ranges[0]),
                        "/** Runs. */"
                    );
                } else {
                    assert!(method.documentation.is_empty());
                    assert!(spelled(&source, method.range).starts_with(assignment));
                }
            }
        }
    }
}

#[test]
fn test_commonjs_local_object_members_keep_callable_owners() {
    let source = "var res = {}; res.clear = function clear() { const opts = { expires: 1 }; return opts.expires; }; res.set = function() { const opts = {}; opts.expires = 2; return opts; }; module.exports = res;";
    for document in [javascript(source), typescript(source)] {
        for name in ["res.clear", "res.set"] {
            let method = symbol(&document, name);
            assert_eq!(method.kind, "method");
            assert_eq!(method.container.as_deref(), Some("res"));
        }
        let property = symbol(&document, "res.clear.opts.expires");
        assert_eq!(
            spelled(source, property.name_range.expect("property name")),
            "expires"
        );
        assert_eq!(symbol(&document, "res.set.opts").kind, "variable");
        assert!(
            !document
                .symbols()
                .iter()
                .any(|symbol| symbol.qualified_name == "res.set.opts.expires")
        );
    }
}

#[test]
fn test_commonjs_prototype_methods_refuse_constructors_with_colliding_lexical_identities() {
    let source = "unknown(function() { function View() {} View.prototype.render = function() {}; }); unknown(function() { function View() {} View.prototype.render = function() {}; });";
    for document in [javascript(source), typescript(source)] {
        assert!(
            !document
                .symbols()
                .iter()
                .any(|symbol| symbol.kind == "method")
        );
    }
}

#[test]
fn test_commonjs_prototype_methods_refuse_replaced_constructor_or_prototype_values() {
    for change in [
        "View = Other;",
        "View.prototype = Other.prototype;",
        "View[key] = other;",
        "eval('replace()');",
    ] {
        let source =
            format!("function View() {{}} {change} View.prototype.render = function() {{}};");
        for document in [javascript(&source), typescript(&source)] {
            assert!(
                !document
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.kind == "method"),
                "{change}"
            );
        }
    }
}

#[test]
fn test_commonjs_prototype_methods_keep_lexical_constructor_owners() {
    let source = "function View() {} View.prototype.render = function render() { const opts = { value: 1 }; };\nfunction nested() { function View() {} View.prototype.lookup = function() {}; }\nfunction shadow(View) { View.prototype.hidden = function() {}; }\nfunction duplicate() { function View() {} function View() {} View.prototype.ambiguous = function() {}; }\nUnknown.prototype.missing = function() {};";
    for document in [javascript(source), typescript(source)] {
        for name in ["View.render", "nested.View.lookup"] {
            let method = symbol(&document, name);
            assert_eq!(method.kind, "method");
            assert_eq!(
                method.container.as_deref(),
                name.rsplit_once('.').map(|(parent, _)| parent)
            );
            assert_eq!(
                spelled(source, method.name_range.expect("method name")),
                method.name
            );
            assert!(spelled(source, method.item_range).contains("prototype"));
            assert!(spelled(source, method.body_range.expect("method body")).starts_with('{'));
        }
        assert_eq!(symbol(&document, "View.render.opts.value").kind, "property");
        assert!(
            !document
                .symbols()
                .iter()
                .any(|symbol| ["hidden", "ambiguous", "missing"].contains(&symbol.name.as_str()))
        );
    }
}

#[test]
fn test_commonjs_function_assignments_keep_names_and_lexical_children() {
    let source = "exports.run = function(value) { const local = {}; };\nexports.other = function inner() { const local = {}; };\nmodule.exports.third = function named() {};\nmodule.exports = function(value) {};";
    for document in [javascript(source), typescript(source)] {
        for (name, token) in [
            ("run", "run"),
            ("other", "other"),
            ("third", "third"),
            ("default", "exports"),
        ] {
            let function = symbol(&document, name);
            assert_eq!(function.kind, "function");
            assert_eq!(
                spelled(source, function.name_range.expect("assignment name")),
                token
            );
            assert!(spelled(source, function.item_range).contains("function"));
        }
        for name in ["run.local", "other.local"] {
            assert_eq!(symbol(&document, name).kind, "variable");
        }
    }
}

#[test]
fn test_typescript_loop_bindings_keep_each_declaration() {
    let document =
        typescript("for (const [leftName, left] of entries) { for (const key in value) { key; } }");
    let names = document
        .symbols()
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["leftName", "left", "key"]);
}

#[test]
fn test_type_literal_members_and_catch_names_keep_their_declarations() {
    let document = typescript(
        "type Result = { value: string; run(): void }; function root() { try {} catch (error) { const value = error; } }",
    );
    for name in ["Result.value", "Result.run", "root.error"] {
        assert!(!symbol(&document, name).name.is_empty());
    }
}

#[test]
fn test_object_members_keep_names_callable_values_and_nested_containers() {
    // #753: object members retain the declaration holding their source.
    let source = "const dictionary = { url: 'URL', validate: (value: string) => value, nested: { value: 1 }, short };";
    let document = typescript(source);
    for name in [
        "dictionary.url",
        "dictionary.validate",
        "dictionary.nested",
        "dictionary.nested.value",
        "dictionary.short",
    ] {
        assert_eq!(symbol(&document, name).kind, "property");
    }
    assert!(
        symbol(&document, "dictionary.validate")
            .kinds
            .contains(&SymbolKind::Callable)
    );
    assert_eq!(
        spelled(
            source,
            symbol(&document, "dictionary.url")
                .name_range
                .expect("member name")
        ),
        "url"
    );
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
    assert!(shape.kinds.contains(&SymbolKind::Abstract));
    assert!(shape.kinds.contains(&SymbolKind::Public));
    let area = symbol(&document, "Shape.area");
    assert!(area.kinds.contains(&SymbolKind::Abstract));
    assert!(area.kinds.contains(&SymbolKind::Callable));
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
            (
                "Shape.constructor.plain",
                Some("Shape.constructor"),
                "value_parameter",
            ),
        ]
    );
    let parameter = symbol(&document, "Shape.constructor.plain");
    assert_eq!(parameter.kinds, [SymbolKind::Value, SymbolKind::Parameter]);
    let sides = symbol(&document, "Shape.sides");
    assert_eq!(sides.kinds, [SymbolKind::Value, SymbolKind::Member]);
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
    assert_eq!(member.kinds, [SymbolKind::Variant]);
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
    for document in [javascript(text), typescript(text)] {
        let names = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.kind,
                    spelled(text, symbol.range),
                    symbol.kinds.contains(&SymbolKind::Public),
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
            ("obj.prop", Some("obj"), "property"),
            ("picked", None, "variable"),
        ]
    );
}

/// The anonymous class a module exports by default declares a class named
/// `default`, the name the module exports it under, and its members keep it
/// as their container.
#[test]
fn test_anonymous_default_class_contains_its_members() {
    let text = "export default class { anon() {} }\n";
    for document in [javascript(text), typescript(text)] {
        assert_eq!(
            declared(&document),
            [
                ("default", None, "class"),
                ("default.anon", Some("default"), "method"),
            ]
        );
        assert!(
            symbol(&document, "default")
                .kinds
                .contains(&SymbolKind::Public)
        );
    }
}

/// A variable holding an arrow function or a function expression can be
/// invoked, so it carries the `callable` category beside `value`.
#[test]
fn test_function_valued_variables_carry_the_callable_category() {
    let text = "export const make = (n) => n;\nconst legacy = function () {};\nconst plain = 1;\n";
    for document in [javascript(text), typescript(text)] {
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kinds.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                (
                    "make",
                    vec![SymbolKind::Value, SymbolKind::Callable, SymbolKind::Public]
                ),
                ("legacy", vec![SymbolKind::Value, SymbolKind::Callable]),
                ("plain", vec![SymbolKind::Value]),
            ]
        );
    }
}

/// Decorators belong to the declaration they qualify: a TypeScript method's
/// decorators, which the grammar writes as siblings before it, start its
/// span, and the `JSDoc` above them documents it, as it does for a class and
/// for a JavaScript method, whose decorators are children of the declaration.
#[test]
fn test_decorated_declarations_span_their_decorators() {
    let text = "/** Sealed. */\n@sealed\nclass Shape {\n  \
                /** Draws. */\n  @log @trace()\n  draw(): void {}\n  \
                @observable sides = 0;\n}\n";
    let document = typescript(text);
    let shape = symbol(&document, "Shape");
    assert_eq!(
        spelled(text, shape.range)
            .lines()
            .take(2)
            .collect::<Vec<_>>(),
        ["/** Sealed. */", "@sealed"]
    );
    let draw = symbol(&document, "Shape.draw");
    assert_eq!(
        spelled(text, draw.range),
        "/** Draws. */\n  @log @trace()\n  draw(): void {}"
    );
    assert_eq!(
        draw.documentation.first().map(|doc| doc.text.as_str()),
        Some("Draws.")
    );
    assert_eq!(
        draw.signatures
            .first()
            .map(|signature| signature.display.as_str()),
        Some("draw(): void")
    );
    assert_eq!(
        spelled(text, symbol(&document, "Shape.sides").range),
        "@observable sides = 0"
    );

    let text = "class Widget {\n  /** Runs. */\n  @bound run() {}\n}\n";
    let document = javascript(text);
    let run = symbol(&document, "Widget.run");
    assert_eq!(spelled(text, run.range), "/** Runs. */\n  @bound run() {}");
    assert_eq!(
        run.documentation.first().map(|doc| doc.text.as_str()),
        Some("Runs.")
    );
}
