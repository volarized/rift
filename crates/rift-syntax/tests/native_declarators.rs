//! Native declaration kinds follow the modifier nearest the declared name.

use rift_core::ProjectPath;
use rift_syntax::{ShippedLanguage, SyntaxLimits, SyntaxSource};

#[test]
fn native_function_prototypes_and_nested_pointer_declarations_keep_distinct_kinds() {
    let text = "int open(void); int *address(void); int (parenthesized)(void); int (*factory(void))(void); int (*callback)(void); int (*callbacks[2])(void); typedef int Port;";
    for shipped in [ShippedLanguage::C, ShippedLanguage::Cpp] {
        let path = ProjectPath::new("client.h").expect("fixture path");
        let document = shipped
            .definition()
            .syntax_provider()
            .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
            .expect("native declarations");
        assert!(!document.has_errors());
        for (name, kind) in [
            ("open", "function"),
            ("address", "function"),
            ("parenthesized", "function"),
            ("factory", "function"),
            ("callback", "variable"),
            ("callbacks", "variable"),
            ("Port", "type"),
        ] {
            let symbol = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("{shipped:?} missing {name}: {:?}", document.symbols()));
            assert_eq!(symbol.kind, kind, "{shipped:?} {name}");
        }
    }
}

#[test]
fn cpp_method_prototypes_and_function_references_keep_distinct_kinds() {
    let text = "struct Client { int open() const; int (*callback)(); }; extern int (&reference)(); extern int (&factory())();";
    let path = ProjectPath::new("client.hpp").expect("fixture path");
    let document = ShippedLanguage::Cpp
        .definition()
        .syntax_provider()
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("C++ declarations");
    assert!(!document.has_errors());
    for (name, kind) in [
        ("open", "function"),
        ("callback", "variable"),
        ("reference", "variable"),
        ("factory", "function"),
    ] {
        let symbol = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", document.symbols()));
        assert_eq!(symbol.kind, kind, "{name}");
    }
}

#[test]
fn every_native_declarator_in_one_declaration_preserves_its_name() {
    for shipped in [ShippedLanguage::C, ShippedLanguage::Cpp] {
        let path = ProjectPath::new("client.h").expect("fixture path");
        let text = "int first = external(4), second = first; int open(void), close(void); int (*callback)(void), value;";
        let document = shipped
            .definition()
            .syntax_provider()
            .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
            .expect("native declarations");
        assert!(!document.has_errors());
        for (name, kind) in [
            ("first", "variable"),
            ("second", "variable"),
            ("open", "function"),
            ("close", "function"),
            ("callback", "variable"),
            ("value", "variable"),
        ] {
            let symbol = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("{shipped:?} missing {name}: {:?}", document.symbols()));
            assert_eq!(symbol.kind, kind, "{shipped:?} {name}");
            let name_range = symbol.name_range.expect("declarator name range");
            let start = usize::try_from(name_range.start).expect("fixture offset");
            let end = usize::try_from(name_range.end).expect("fixture offset");
            assert_eq!(&text[start..end], name);
            let start = usize::try_from(symbol.range.start).expect("fixture offset");
            let end = usize::try_from(symbol.range.end).expect("fixture offset");
            assert!(text[start..end].ends_with(';'), "whole declaration: {name}");
        }
        assert!(
            !document
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "external")
        );
    }
}

#[test]
fn cython_multiple_names_exclude_initializer_references() {
    let path = ProjectPath::new("client.pyx").expect("fixture path");
    let text = "cdef int first = external(4), second = first\ncdef int third, fourth\n";
    let document = ShippedLanguage::Cython
        .definition()
        .syntax_provider()
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("Cython declarations");
    assert!(!document.has_errors());
    for name in ["first", "second", "third", "fourth"] {
        assert!(
            document.symbols().iter().any(|symbol| symbol.name == name),
            "missing {name}: {:?}",
            document.symbols()
        );
    }
    assert!(
        !document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "external")
    );
    assert_eq!(document.symbols().len(), 4);
}

#[test]
fn qualified_cpp_prototype_and_cython_pointer_keep_declaration_and_name_ranges() {
    let cases = [
        (
            ShippedLanguage::Cpp,
            "client.hpp",
            "int Widget::beam(void);",
            "int Widget::beam(void);",
            &[("beam", "function")][..],
        ),
        (
            ShippedLanguage::Cython,
            "client.pyx",
            "cdef int first, *second\n",
            "int first, *second",
            &[("first", "variable"), ("second", "variable")][..],
        ),
    ];
    for (shipped, path, text, declaration, expected) in cases {
        let path = ProjectPath::new(path).expect("fixture path");
        let document = shipped
            .definition()
            .syntax_provider()
            .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
            .expect("native declarations");
        assert!(!document.has_errors());
        assert_eq!(document.symbols().len(), expected.len());
        for (name, kind) in expected {
            let symbol = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == *name)
                .unwrap_or_else(|| panic!("missing {name}: {:?}", document.symbols()));
            assert_eq!(symbol.kind, *kind);
            let name_range = symbol.name_range.expect("declarator name range");
            let start = usize::try_from(name_range.start).expect("fixture offset");
            let end = usize::try_from(name_range.end).expect("fixture offset");
            assert_eq!(&text[start..end], *name);
            let start = usize::try_from(symbol.range.start).expect("fixture offset");
            let end = usize::try_from(symbol.range.end).expect("fixture offset");
            assert_eq!(text[start..end].trim(), declaration);
        }
    }
}

#[test]
fn wide_native_declarations_keep_every_name_without_cross_parse_state() {
    const COUNT: usize = 2048;
    let path = ProjectPath::new("wide.h").expect("fixture path");
    let names = (0..COUNT).map(|index| format!("value{index} = external({index})"));
    let text = format!("int {};", names.collect::<Vec<_>>().join(", "));
    for shipped in [ShippedLanguage::C, ShippedLanguage::Cpp] {
        let provider = shipped.definition().syntax_provider();
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: &text,
                },
                SyntaxLimits::default(),
            )
            .expect("wide native declaration");
        assert!(!document.has_errors());
        assert_eq!(document.symbols().len(), COUNT);
        for (index, symbol) in document.symbols().iter().enumerate() {
            assert_eq!(symbol.name, format!("value{index}"));
            assert_eq!(symbol.kind, "variable");
        }
        let next = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "int open(void), close(void);",
                },
                SyntaxLimits::default(),
            )
            .expect("independent parse");
        assert_eq!(next.symbols().len(), 2);
        assert!(
            next.symbols()
                .iter()
                .all(|symbol| symbol.kind == "function")
        );
    }
}
